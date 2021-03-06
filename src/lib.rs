// Copyright 2015 Dawid Ciężarkiewicz <dpc@dpc.pw>
// See LICENSE-MPL2 file for more information.

//! # Mioco
//!
//! Scalable, coroutine-based, asynchronous IO handling library for Rust
//! programming language.
//!
//! Using `mioco` you can handle asynchronous [`mio`][mio]-based IO, using
//! set of synchronous-IO handling functions. Based on [`mio`][mio] events
//! `mioco` will cooperatively schedule your handlers.
//!
//! You can think of `mioco` as of *Node.js for Rust* or *[green threads][green threads] on top of [`mio`][mio]*.
//!
//! `Mioco` is a library building on top of [`mio`][mio]. Mio API is
//! re-exported as [`mioco::mio`][mio-api].
//!
//! # <a name="features"></a> Features:
//!
//! ```norust
//! * multithreading support; (see `Config::set_thread_num()`)
//! * user-provided scheduling; (see `Config::set_scheduler()`);
//! * support for all native `mio` types (see `MiocoHandle::wrap()`);
//! * timers (see `MiocoHandle::timer()`);
//! * mailboxes (see `mailbox()`);
//! * coroutine exit notification (see `CoroutineHandle::exit_notificator()`).
//! * synchronous operations support (see `MiocoHandle::sync()`).
//! ```
//!
//! # <a name="example"/></a> Example:
//!
//! See `examples/echo.rs` for an example TCP echo server:
//!
/*!
```
// MAKE_DOC_REPLACEME
```
*/
//! [green threads]: https://en.wikipedia.org/wiki/Green_threads
//! [mio]: https://github.com/carllerche/mio
//! [mio-api]: ../mioco/mio/index.html

#![cfg_attr(test, feature(convert))]
#![feature(reflect_marker)]
#![feature(catch_panic)]
#![feature(raw)]
#![feature(drain)]
#![feature(fnbox)]
#![warn(missing_docs)]

#[cfg(test)]
extern crate env_logger;

extern crate thread_scoped;
extern crate libc;
extern crate spin;
extern crate mio as mio_orig;
extern crate context;
extern crate nix;
#[macro_use]
extern crate log;
extern crate bit_vec;
extern crate time;
extern crate num_cpus;
extern crate slab;

/// Re-export of all `mio` symbols.
///
/// Use that instead this to access plain-`mio` types.
pub use mio_orig as mio;

use std::cell::RefCell;
use std::rc::{Rc};
use std::io;
use std::thread;
use std::mem::{self, transmute, size_of_val};
use std::raw::TraitObject;

use std::boxed::FnBox;

use mio::{TryRead, TryWrite, Token, EventLoop, EventSet, EventLoopConfig};
use mio::udp::{UdpSocket};
use std::net::SocketAddr;
use std::any::Any;
use std::marker::{PhantomData, Reflect};

use std::collections::VecDeque;
use spin::Mutex;
use std::sync::{Arc};
use std::sync::atomic::{AtomicUsize, Ordering};

use slab::{Slab, SlabMutIter};

use bit_vec::BitVec;

use time::{SteadyTime, Duration};

use context::{Context, Stack};

use std::ptr;
thread_local!(static TL_CURRENT_COROUTINE: RefCell<*mut Coroutine> = RefCell::new(ptr::null_mut()));

use Message::*;

type RcCoroutine = Rc<RefCell<Coroutine>>;
type RcEventSourceShared = Rc<RefCell<EventSourceShared>>;
type ArcMailboxShared<T> = Arc<Mutex<MailboxShared<T>>>;
type RcHandlerShared = Rc<RefCell<HandlerShared>>;
type ArcHandlerThreadShared = Arc<HandlerThreadShared>;

/// Read/Write/Both
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct RW {
    read : bool,
    write : bool,
}

impl RW {
    fn read() -> Self {
        RW {
            read: true,
            write: false,
        }
    }

    fn write() -> Self {
        RW {
            read: false,
            write: true,
        }
    }

    fn both() -> Self {
        RW {
            read: true,
            write: true,
        }
    }

    fn none() -> Self {
        RW {
            read: false,
            write: false,
        }
    }

    fn as_tuple(&self) -> (bool, bool) {
        (self.read, self.write)
    }

    fn has_read(&self) -> bool {
        self.read
    }

    fn has_none(&self) -> bool {
        !self.read && !self.write
    }

    fn has_write(&self) -> bool {
        self.write
    }
}


/// Event delivered to the coroutine
///
/// Read and/or Write + event source ID
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Event {
    id : EventSourceId,
    rw : RW,
}

impl Event {
    /// Index of the EventSourceShared handle
    pub fn id(&self) -> EventSourceId {
        self.id
    }

    /// Was the event a read
    pub fn has_read(&self) -> bool {
        self.rw.has_read()
    }

    /// Was the event a write
    pub fn has_write(&self) -> bool {
        self.rw.has_write()
    }
}

/// Coroutine exit status (value returned or panic)
#[derive(Clone, Debug)]
pub enum ExitStatus {
    /// Coroutine panicked
    Panic,
    /// Killed externally
    Killed,
    /// Coroutine returned some value
    Exit(Arc<io::Result<()>>)
}

impl ExitStatus {
    /// Is the `ExitStatus` a `Panic`?
    pub fn is_panic(&self) -> bool {
        match *self {
            ExitStatus::Panic => true,
            _ => false,
        }
    }
}

/// State of `mioco` coroutine
#[derive(Clone, Debug)]
enum State {
    /// Blocked on RW
    BlockedOn(RW),
    /// Need to unregister EventSource
    UnregisterdEventSource(EventSourceId),
    /// Currently running
    Running,
    /// Ready to be started
    Ready,
    /// Finished
    Finished(ExitStatus),
}

impl State {
    /// Is the `State` a `Finished(_)`?
    fn is_finished(&self) -> bool {
        match *self {
            State::Finished(_) => true,
            _ => false,
        }
    }

    /// Is the `State` `Ready`?
    fn is_ready(&self) -> bool {
        match *self {
            State::Ready => true,
            _ => false,
        }
    }

    /// Is the `State` `Running`?
    fn is_running(&self) -> bool {
        match *self {
            State::Running => true,
            _ => false,
        }
    }

    /// Is the `State` `Blocked`?
    fn is_blocked(&self) -> bool {
        match *self {
            State::BlockedOn(_) => true,
            _ => false,
        }
    }

    /// Is the `State` `UnregisterdEventSource`?
    fn is_unregister_eventsource(&self) -> bool {
        match *self {
            State::UnregisterdEventSource(_) => true,
            _ => false,
        }
    }
}

/// Sends notify `Message` to the mioco Event Loop.
type MioSender = mio::Sender<<Handler as mio::Handler>::Message>;

/// `mioco` can work on any type implementing this trait
pub trait Evented : Any {
    /// Convert to &Any
    fn as_any(&self) -> &Any;
    /// Convert to &mut Any
    fn as_any_mut(&mut self) -> &mut Any;

    /// Register
    fn register(&self, event_loop : &mut EventLoop<Handler>, token : Token, interest : EventSet);

    /// Reregister
    fn reregister(&self, event_loop : &mut EventLoop<Handler>, token : Token, interest : EventSet);

    /// Deregister
    fn deregister(&self, event_loop : &mut EventLoop<Handler>, token : Token);

    /// Should the coroutine be resumed on event for this `EventSource<Self>`
    fn should_resume(&self) -> bool {
        true
    }
}

impl<T> Evented for T
where T : mio::Evented+Reflect+'static {
    fn as_any(&self) -> &Any {
        self as &Any
    }

    fn as_any_mut(&mut self) -> &mut Any {
        self as &mut Any
    }

    fn register(&self, event_loop : &mut EventLoop<Handler>, token : Token, interest : EventSet) {
        trace!("Evented({}): register", token.as_usize());
        event_loop.register(
            self, token,
            interest,
            mio::PollOpt::edge(),
            ).expect("register_opt failed");
    }

    fn reregister(&self, event_loop : &mut EventLoop<Handler>, token : Token, interest : EventSet) {
        trace!("Evented({}): reregister", token.as_usize());
        event_loop.reregister(
            self, token,
            interest,
            mio::PollOpt::edge(),
            ).expect("reregister failed");
    }

    fn deregister(&self, event_loop : &mut EventLoop<Handler>, token : Token) {
        trace!("Evented({}): deregister", token.as_usize());
        event_loop.deregister(self).expect("deregister failed");
    }
}

/// Retry `mio::Sender::send()`.
///
/// As channels can fail randomly (eg. when Full), take care
/// of retrying on recoverable errors.
fn sender_retry<M : Send>(sender : &mio::Sender<M>, msg : M) {
    let mut msg = Some(msg);
    let mut warning_printed = false;
    loop {
        match sender.send(msg.take().expect("sender_retry")) {
            Ok(()) => break,
            Err(mio::NotifyError::Closed(_)) => panic!("Closed channel on sender.send()."),
            Err(mio::NotifyError::Io(_)) => panic!("IO error on sender.send()."),
            Err(mio::NotifyError::Full(retry_msg)) => {
                msg = Some(retry_msg);
            },
        }
        if !warning_printed {
            warning_printed = true;
            warn!("send_retry: retry; consider increasing `EventLoopConfig::notify_capacity`");
        }
        thread::yield_now();
    }
}

impl<T> Evented for MailboxInnerEnd<T>
where T:Reflect+'static {
    fn as_any(&self) -> &Any {
        self as &Any
    }

    fn as_any_mut(&mut self) -> &mut Any {
        self as &mut Any
    }

    fn register(&self, event_loop : &mut EventLoop<Handler>, token : Token, interest : EventSet) {
        trace!("MailboxInnerEnd({}): register", token.as_usize());
        let mut lock = self.shared.lock();

        lock.token = Some(token);
        lock.sender = Some(event_loop.channel());
        lock.interest = interest;

        if interest.is_readable() && !lock.inn.is_empty() {
            trace!("MailboxInnerEnd({}): not empty; self notify", token.as_usize());
            lock.interest = EventSet::none();
            sender_retry(lock.sender.as_ref().unwrap(), MailboxMsg(token));
        }
    }

    fn reregister(&self, _event_loop : &mut EventLoop<Handler>, token : Token, interest : EventSet) {
        trace!("MailboxInnerEnd({}): reregister", token.as_usize());
        let mut lock = self.shared.lock();

        lock.interest = interest;

        if interest.is_readable() && !lock.inn.is_empty() {
            lock.interest = EventSet::none();
            sender_retry(lock.sender.as_ref().unwrap(), MailboxMsg(token));
        }
    }

    fn deregister(&self, _event_loop : &mut EventLoop<Handler>, token : Token) {
        trace!("MailboxInnerEnd({}): dereregister", token.as_usize());
        let mut lock = self.shared.lock();
        lock.token = None;
        lock.sender = None;
        lock.interest = EventSet::none();
    }

    fn should_resume(&self) -> bool {
        let lock = self.shared.lock();
        trace!("MailboxInnerEnd: should_resume? {}", !lock.inn.is_empty());
        !lock.inn.is_empty()
    }
}

impl Evented for Timer {
    fn as_any(&self) -> &Any {
        self as &Any
    }

    fn as_any_mut(&mut self) -> &mut Any {
        self as &mut Any
    }

    fn register(&self, event_loop : &mut EventLoop<Handler>, token : Token, _interest : EventSet) {
        let timeout = self.timeout;
        let now = SteadyTime::now();
        let delay = if timeout <= now {
            0
        } else {
            (timeout - now).num_milliseconds()
        };

        trace!("Timer({}): set timeout in {}ms", token.as_usize(), delay);
        match event_loop.timeout_ms(token, delay as u64) {
            Ok(_) => {},
            Err(reason)=> {
                panic!("Could not create mio::Timeout: {:?}", reason);
            }
        }
    }

    fn reregister(&self, event_loop : &mut EventLoop<Handler>, token : Token, interest : EventSet) {
        self.register(event_loop, token, interest)
    }

    fn deregister(&self, _event_loop : &mut EventLoop<Handler>, _token : Token) {
    }

    fn should_resume(&self) -> bool {
        trace!("Timer: should_resume? {}", self.timeout <= SteadyTime::now());
        self.timeout <= SteadyTime::now()
    }
}

/// Mioco coroutine (a.k.a. *mioco handler*)
pub struct Coroutine {
    /// Context with a state of coroutine
    context: Context,

    /// Current state
    state : State,

    /// Last event that resumed the coroutine
    last_event: Event,

    /// `Handler` shared data that this `Coroutine` is running in
    handler_shared : Option<RcHandlerShared>,

    /// Mask of handle ids that we're blocked on
    blocked_on : BitVec<usize>,

    // TODO: Move to Coroutine
    /// Mask of handle ids that are registered in Handler
    registered : BitVec<usize>,

    /// `Coroutine` will send exit status on it's finish
    /// through this list of Mailboxes
    exit_notificators : Vec<MailboxOuterEnd<ExitStatus>>,

    /// Current coroutine Id
    id : CoroutineId,

    /// Coroutine stack
    stack: Stack,

    /// All event sources
    io : Slab<RcEventSourceShared, EventSourceId>,

    /// Newly spawned `Coroutine`-es
    children_to_start : Vec<RcCoroutine>,

    /// Function to be run inside Coroutine
    coroutine_func : Option<Box<FnBox() -> io::Result<()> + Send + 'static>>,

    /// In case Rc to self is needed
    self_rc : Option<RcCoroutine>,

    timer : Option<EventSource<Timer>>,

    sync_mailbox: Option<(MailboxOuterEnd<()>, EventSource<MailboxInnerEnd<()>>)>,
}

/// Mioco Handler keeps only Slab of Coroutines, and uses a scheme in which
/// Token bits encode both Coroutine and EventSource within it
const EVENT_SOURCE_TOKEN_SHIFT : usize = 10;
const EVENT_SOURCE_TOKEN_MASK : usize = (1 << EVENT_SOURCE_TOKEN_SHIFT) - 1;

/// Convert token to ids
fn token_to_ids(token : Token) -> (CoroutineId, EventSourceId) {
    let val = token.as_usize();
    (
        CoroutineId(val >> EVENT_SOURCE_TOKEN_SHIFT),
        EventSourceId(val & EVENT_SOURCE_TOKEN_MASK),
    )
}

/// Convert ids to Token
fn token_from_ids(co_id : CoroutineId, io_id : EventSourceId) -> Token {
    // TODO: Add checks on wrap()
    debug_assert!(io_id.as_usize() <= EVENT_SOURCE_TOKEN_MASK);
    Token((co_id.as_usize() << EVENT_SOURCE_TOKEN_SHIFT) | io_id.as_usize())
}

/// Event delivery point, kept in Handler slab.
#[derive(Clone)]
struct CoroutineSlabHandle {
    rc : RcCoroutine,
}

impl CoroutineSlabHandle {
    fn new(rc : RcCoroutine) -> Self {
        CoroutineSlabHandle {
            rc: rc,
        }
    }

    fn to_coroutine_control(self) -> CoroutineControl {
        CoroutineControl::new(self.rc)
    }

    /// Deliver an event to a Coroutine
    fn event(
        &self,
        event_loop : &mut EventLoop<Handler>,
        token : Token,
        events : EventSet,
        ) -> bool {
        let (_, io_id) = token_to_ids(token);

        trace!("Coroutine({}): event", self.id().as_usize());
        let (should_resume, should_reregister) = {
            let mut coroutine = self.rc.borrow_mut();
            let io = coroutine.io.get(io_id).unwrap().clone();

            if events.is_hup() {
                io.borrow_mut().hup(event_loop, token);
            }

            coroutine.registered.set(io_id.as_usize(), false);

            if let State::BlockedOn(rw) = coroutine.state {

                if !coroutine.blocked_on.get(io_id.as_usize()).unwrap() {
                    // spurious event, probably after select in which
                    // more than one event sources were reported ready
                    // in one group of events, and first event source
                    // deregistered the later ones
                    debug!("spurious event for event source coroutine is not blocked on");
                    (false, false)
                } else {
                    match rw.as_tuple() {
                        (false, false) => {
                            debug!("spurious event for coroutine blocked on nothing");
                            (false, false)
                        },
                        (true, false) if !events.is_readable() && !events.is_hup() => {
                            debug!("spurious not read event for coroutine blocked on read");
                            (false, false)
                        },
                        (false, true) if !events.is_writable() => {
                            debug!("spurious not write event for coroutine blocked on write");
                            (false, false)
                        },
                        (true, true) if !events.is_readable() && !events.is_hup() && !events.is_writable() => {
                            debug!("spurious unknown type event for coroutine blocked on read/write");
                            (false, false)
                        },
                        _ => {
                            if io.borrow().io.should_resume() {
                                (true, false)
                            } else {
                                // TODO: Actually, we can just reregister the Timer,
                                // not all sources, and in just this one case
                                (false, true)
                            }
                        }
                    }
                }
            } else {
                // subsequent event to coroutine that is either already
                // Finished, or Ready
                (false, false)
            }
        };

        if should_resume {
            trace!("Coroutine({}): set to ready", self.id().as_usize());
            // Wake coroutine on HUP, as it was read, to potentially let it fail the read and move on
            let event = match (events.is_readable() | events.is_hup(), events.is_writable()) {
                (true, true) => RW::both(),
                (true, false) => RW::read(),
                (false, true) => RW::write(),
                (false, false) => panic!(),
            };

            let mut co = self.rc.borrow_mut();
            co.state = State::Ready;
            co.last_event = Event {
                rw: event,
                id: io_id,
            };
            true
        } else if should_reregister {
            trace!("Coroutine({}): event ignored (reregister)", self.id().as_usize());
            // Wake coroutine on HUP, as it was read, to potentially let it fail the read and move on
            self.after_resume(event_loop);
            false
        } else {
            trace!("Coroutine({}): event ignored (no reregister)", self.id().as_usize());
            false
        }
    }

    fn id(&self) -> CoroutineId {
        let coroutine = self.rc.borrow();
        coroutine.id
    }

    /// After `resume()` (or ignored event()) we need to perform the following maintenance
    fn after_resume(
        &self,
        event_loop: &mut EventLoop<Handler>,
        ) {
        // Take care of newly spawned child-coroutines: start them now
        debug_assert!(!self.rc.borrow().state().is_running());

        self.rc.borrow_mut().reregister(event_loop);

        {

            let Coroutine {
                ref mut children_to_start,
                ref handler_shared,
                ref id,
                ..
            } = *self.rc.borrow_mut();

            trace!("Coroutine({}): {} children spawned", id.as_usize(), children_to_start.len());

            let mut handler_shared = handler_shared.as_ref().unwrap().borrow_mut();

            for coroutine in children_to_start.drain(..) {
                let coroutine_ctrl = CoroutineControl::new(coroutine);
                handler_shared.spawned.push(coroutine_ctrl);
            }
        }

        let state = self.rc.borrow().state();
        if let State::BlockedOn(rw) = state {
            if rw.has_none() {
                let mut coroutine_ctrl = CoroutineControl::new(self.rc.clone());
                coroutine_ctrl.set_is_yielding();
                self.rc.borrow_mut().state = State::Ready;
                let handler_shared = &self.rc.borrow().handler_shared;
                let mut handler_shared = handler_shared.as_ref().unwrap().borrow_mut();

                handler_shared.ready.push(coroutine_ctrl);
            }
        }
    }
}



/// Coroutine control block
///
/// Through this interface Coroutine can be resumed and migrated.
pub struct CoroutineControl {
    /// In case `CoroutineControl` gets dropped in `SchedulerThread` Drop
    /// trait will kill the Coroutine
    was_handled : bool,
    is_yielding : bool,
    rc : RcCoroutine,
}

impl Drop for CoroutineControl {
    fn drop(&mut self) {
        if !self.was_handled {
            trace!("Coroutine({}): kill", self.id().as_usize());
            self.rc.borrow_mut().state = State::Finished(ExitStatus::Killed);
            coroutine_jump_in(&self.rc);
        }
    }
}

impl CoroutineControl {
    fn new(rc : RcCoroutine) -> Self {
        CoroutineControl {
            is_yielding: false,
            was_handled: false,
            rc: rc,
        }
    }

    // TODO: Eliminate this needles clone()
    fn to_slab_handle(&self) -> CoroutineSlabHandle {
        CoroutineSlabHandle::new(self.rc.clone())
    }

    /// Resume Coroutine
    ///
    /// Panics if Coroutine is not in Ready state.
    pub fn resume(
        mut self,
        event_loop : &mut EventLoop<Handler>,
        ) {
        self.was_handled = true;
        trace!("Coroutine({}): resume", self.id().as_usize());
        let co_rc = self.rc.clone();
        let is_ready = co_rc.borrow().state.is_ready();
        if is_ready {
            loop {
                coroutine_jump_in(&co_rc);
                let state = co_rc.borrow().state.clone();
                if let State::UnregisterdEventSource(index) = state {
                    let mut co = co_rc.borrow_mut();
                    {
                        let io = co.io.get(index).unwrap();
                        io.borrow_mut().deregister(event_loop, co.id);
                    }
                    co.registered.set(index.as_usize(), false);
                    co.blocked_on.set(index.as_usize(), false);
                    co.state = State::Ready;
                } else {
                    break;
                }
            }
            self.to_slab_handle().after_resume(event_loop);
        } else {
            panic!("Tried to resume Coroutine that is not ready");
        }
    }

    fn id(&self) -> CoroutineId {
       self.rc.borrow().id
    }

    /// Migrate to a different thread
    ///
    /// Move this Coroutine to be executed on a `SchedulerThread` for a
    /// given `thread_id`.
    ///
    /// Will panic if `thread_id` is not valid.
    pub fn migrate(
        mut self,
        event_loop : &mut EventLoop<Handler>,
        thread_id : usize,
        ) {
        self.was_handled = true;
        let sender = {
            trace!("Coroutine({}): migrate to thread {}", self.id().as_usize(), thread_id);
            let mut co = self.rc.borrow_mut();
            co.deregister_all(event_loop);
            co.registered.clear();

            let id = co.id;
            // TODO: https://github.com/contain-rs/bit-vec/pulls
            co.blocked_on.clear();
            let handler_shared = co.handler_shared.take();
            debug_assert!(co.handler_shared.is_none());
            let mut handler_shared = handler_shared.as_ref().unwrap().borrow_mut();
            handler_shared.coroutines.remove(id).unwrap();
            handler_shared.senders[thread_id].clone()
        };

        let rc = self.rc.clone();

        drop(self);

        // TODO: Spin on failure
        sender_retry(&sender, Migration(rc));
    }


    /// Finish migrating Coroutine by attaching it to a new thread
    fn reattach_to(
        &mut self,
        handler : &mut Handler,
        ) {
        let handler_shared = handler.shared.clone();

        trace!("Coroutine({}): reattach in a new thread", self.id().as_usize());
        let coroutines = &mut handler.shared.borrow_mut().coroutines;

        if !coroutines.has_remaining() {
            let count = coroutines.count();
            coroutines.grow(count);
        }

        let _id = coroutines.insert_with(|id| {
            let mut co = self.rc.borrow_mut();

            co.id = id;
            co.handler_shared = Some(handler_shared);

            CoroutineSlabHandle::new(self.rc.clone())
        }).expect("Run out of slab for coroutines");
    }

    fn set_is_yielding(&mut self) {
        self.is_yielding = true
    }


    /// Is this Coroutine ready after `yield_now()`?
    pub fn is_yielding(&self) -> bool {
        self.is_yielding
    }
}

impl Coroutine {
    /// Spawn a new Coroutine
    fn spawn<F>(handler_shared : RcHandlerShared, f : F) -> RcCoroutine
    where F : FnOnce() -> io::Result<()> + Send + 'static {
        trace!("Coroutine: spawning");
        let stack_size = handler_shared.borrow().stack_size;

        let id = {
            let coroutines = &mut handler_shared.borrow_mut().coroutines;

            if !coroutines.has_remaining() {
                let count = coroutines.count();
                coroutines.grow(count);
            }

            coroutines.insert_with(|id| {
                let coroutine = Coroutine {
                    state: State::Ready,
                    id: id,
                    last_event: Event{ rw: RW::read(), id: EventSourceId(0)},
                    context: Context::empty(),
                    handler_shared: Some(handler_shared.clone()),
                    blocked_on: Default::default(),
                    exit_notificators: Vec::new(),
                    registered: Default::default(),
                    io: Slab::new(4),
                    children_to_start: Vec::new(),
                    stack: Stack::new(stack_size),
                    coroutine_func: Some(Box::new(f)),
                    self_rc: None,
                    timer: None,
                    sync_mailbox: None,
                };

                CoroutineSlabHandle::new(Rc::new(RefCell::new(coroutine)))
            }).expect("Run out of slab for coroutines")
        };
        handler_shared.borrow_mut().coroutines_inc();

        let coroutine_rc = handler_shared.borrow().coroutines[id].rc.clone();

        coroutine_rc.borrow_mut().self_rc = Some(coroutine_rc.clone());

        let coroutine_ptr = {
            // The things we do for borrowck...
            let coroutine_ptr = {
                &*coroutine_rc.borrow() as *const Coroutine
            };
            coroutine_ptr
        };

        extern "C" fn init_fn(arg: usize, _: *mut libc::types::common::c95::c_void) -> ! {
            let ctx : &Context = {

                let res : Result<(), _> = thread::catch_panic(
                    move|| {
                        let coroutine : &mut Coroutine = unsafe { transmute(arg) };
                        trace!("Coroutine({}): started", {
                            coroutine.id.as_usize()
                        });

                        entry_point(coroutine.self_rc.as_ref().unwrap());
                        let f = coroutine.coroutine_func.take().unwrap();

                        // TODO: Weird syntax...
                        // FIXME?
                        let _ = f.call_box(());
                    }
                    );

                let coroutine : &mut Coroutine = unsafe { transmute(arg) };
                coroutine.io.clear();
                coroutine.self_rc = None;

                let id = coroutine.id;
                // TODO: https://github.com/contain-rs/bit-vec/pulls
                coroutine.blocked_on.clear();
                {
                    let mut handler_shared = coroutine.handler_shared.as_ref().unwrap().borrow_mut();
                    handler_shared.coroutines.remove(id).unwrap();
                    handler_shared.coroutines_dec();
                }

                match res {
                    Ok(res) => {
                        trace!("Coroutine({}): finished returning {:?}", id.as_usize(), res);
                        let arc_res = Arc::new(Ok(res));
                        coroutine.exit_notificators.iter().map(
                            |end| end.send(ExitStatus::Exit(arc_res.clone()))
                            ).count();
                        coroutine.state = State::Finished(ExitStatus::Exit(arc_res));

                    },
                    Err(cause) => {
                        trace!("Coroutine({}): panicked: {:?}", id.as_usize(), cause.downcast::<&str>());
                        if let State::Finished(ExitStatus::Killed) = coroutine.state {
                            coroutine.exit_notificators.iter().map(
                                |end| end.send(ExitStatus::Killed)
                                ).count();
                        } else {
                            coroutine.state = State::Finished(ExitStatus::Panic);
                            coroutine.exit_notificators.iter().map(
                                |end| end.send(ExitStatus::Panic)
                                ).count();
                        }
                    }
                }

                unsafe {
                    let handler = coroutine.handler_shared.as_ref().unwrap().borrow();
                    transmute(&handler.context as *const Context)
                }
            };

            Context::load(ctx);

            unreachable!();
        }

        {
            let Coroutine {
                ref mut stack,
                ref mut context,
                ..
            } = *coroutine_rc.borrow_mut();

            context.init_with(
                init_fn,
                coroutine_ptr as usize,
                std::ptr::null_mut(),
                stack,
                );
        }

        coroutine_rc
    }

    fn state(&self) -> State {
        self.state.clone()
    }

    fn reregister(&mut self, event_loop: &mut EventLoop<Handler>) {
        if self.state().is_finished() {
            trace!("Coroutine: deregistering");
            self.deregister_all(event_loop);
        } else {
            self.reregister_blocked_on(event_loop)
        }
    }

    fn deregister_all(&mut self, event_loop: &mut EventLoop<Handler>) {
        for io in self.io.iter() {
            io.borrow_mut().deregister(event_loop, self.id);
        }
    }

    fn reregister_blocked_on(&mut self, event_loop: &mut EventLoop<Handler>) {

        let rw = match self.state {
            State::BlockedOn(rw) => rw,
            _ => panic!("This should not happen"),
        };

        let Coroutine {
            id,
            ref mut io,
            ref mut blocked_on,
            ref mut registered,
            ..
        } = *self;
        {
            let mut i = 0;
            for (registered_block, blocked_on_block) in registered.blocks().zip(blocked_on.blocks()) {
                debug_assert!(size_of_val(&registered_block) == size_of_val(&blocked_on_block));
                let bit_size = size_of_val(&registered_block) * 8;
                let mut block = registered_block ^ blocked_on_block;
                'for_each_set_bit: loop {
                    let lz = block.leading_zeros() as usize;
                    if lz == bit_size {
                        break 'for_each_set_bit
                    } else {
                        let bit = bit_size - 1 - lz;
                        debug_assert!(bit < bit_size);
                        block &= !(1 << bit);
                        let mut io = io[EventSourceId(i + bit)].borrow_mut();
                        if registered_block & (1 << bit) != 0 {
                            debug_assert!(blocked_on_block & (1 << bit) == 0);
                            io.unreregister(event_loop, id);
                        } else {
                            debug_assert!(blocked_on_block & (1 << bit) != 0);
                            io.reregister(event_loop, id, rw);
                        }
                    }
                }
                i += bit_size;
            }
        }


        // effectively: self.registered = self.blocked_on;
        for (mut target, src) in unsafe { registered.storage_mut().iter_mut().zip(blocked_on.storage().iter()) } {
            *target = *src
        }
    }

    fn unwrap_event_source<T>(&mut self, io : &mut EventSource<T>) -> RcEventSourceShared {
        let index = io.inn.borrow().id;
        let registered = io.inn.borrow().registered;

        if registered {
            self.state = State::UnregisterdEventSource(index);
            coroutine_jump_out(&self.self_rc.as_ref().unwrap());
            debug_assert!(!io.inn.borrow().registered);
        }

        io.unwrapped = true;
        self.io.remove(index).unwrap()
    }
}

/// Resume coroutine execution, jumping into it
fn coroutine_jump_in(coroutine : &RefCell<Coroutine>) {
    let prev = TL_CURRENT_COROUTINE.with( |co| {
        let mut co = co.borrow_mut();
        let prev = *co;
        *co = &mut *coroutine.borrow_mut() as *mut Coroutine;
        prev
    });

    {
        let ref mut state = coroutine.borrow_mut().state;
        match *state {
            State::Ready => {
                *state = State::Running;
            },
            State::Finished(ExitStatus::Killed) => {} ,
            ref state => panic!("coroutine_jump_in: wrong state {:?}", state),
        }
    }

    // We know that we're holding at least one Rc to the Coroutine,
    // and noone else is holding a reference as we can do `.borrow_mut()`
    // so we cheat with unsafe just to context-switch from coroutine
    // without having RefCells still borrowed.
    let (context_in, context_out) = {
        let Coroutine {
            ref context,
            ref handler_shared,
            ..
        } = *coroutine.borrow_mut();
        {
            let mut shared_context = &mut handler_shared.as_ref().unwrap().borrow_mut().context;
            (context as *const Context, shared_context as *mut Context)
        }
    };

    Context::swap(unsafe {&mut *context_out}, unsafe {&*context_in});
    TL_CURRENT_COROUTINE.with( |co| {
        *co.borrow_mut() = prev;
    });
}

/// Coroutine entry point checks
fn entry_point(coroutine : &RefCell<Coroutine>) {
    if let State::Finished(ExitStatus::Killed) = coroutine.borrow().state {
        panic!("Killed externally")
    }
}

/// Block coroutine execution, jumping out of it
fn coroutine_jump_out(coroutine : &RefCell<Coroutine>) {
    {
        let state = &coroutine.borrow().state;
        debug_assert!(state.is_blocked() || state.is_unregister_eventsource());
    }

    // See `resume()` for unsafe comment
    let (context_in, context_out) = {
        let Coroutine {
            ref mut context,
            ref handler_shared,
            ..
        } = *coroutine.borrow_mut();
        {
            let shared_context = &mut handler_shared.as_ref().unwrap().borrow_mut().context;
            (context as *mut Context, shared_context as *const Context)
        }
    };

    Context::swap(unsafe {&mut *context_in}, unsafe {&*context_out});
}

/// Wrapped mio IO (mio::Evented+TryRead+TryWrite)
///
/// `Handle` is just a cloneable reference to this struct
struct EventSourceShared {
    coroutine : RcCoroutine,
    id : EventSourceId,
    io : Box<Evented+'static>,
    peer_hup: bool,
    registered: bool,
}

impl EventSourceShared {
    /// Handle `hup` condition
    fn hup(&mut self, _event_loop: &mut EventLoop<Handler>, _token: Token) {
        trace!("hup");
        self.peer_hup = true;
    }

    /// Reregister oneshot handler for the next event
    fn reregister(&mut self, event_loop: &mut EventLoop<Handler>, co_id : CoroutineId, rw : RW) {
            let mut interest = mio::EventSet::none();

            if !self.peer_hup {
                interest = interest | mio::EventSet::hup();

                if rw.has_read() {
                    interest = interest | mio::EventSet::readable();
                }
            }

            if rw.has_write() {
                interest = interest | mio::EventSet::writable();
            }

            let token = token_from_ids(co_id, self.id);

            if !self.registered {
                self.registered = true;
                Evented::register(&*self.io, event_loop, token, interest);
            } else {
                Evented::reregister(&*self.io, event_loop, token, interest);
             }
        }

    /// Un-reregister events we're not interested in anymore
    fn unreregister(&self, event_loop: &mut EventLoop<Handler>, co_id : CoroutineId) {
            debug_assert!(self.registered);
            let interest = mio::EventSet::none();
            let token = token_from_ids(co_id, self.id);
            Evented::reregister(&*self.io, event_loop, token, interest);
        }

    /// Un-reregister events we're not interested in anymore
    fn deregister(&mut self, event_loop: &mut EventLoop<Handler>, co_id : CoroutineId) {
            if self.registered {
                let token = token_from_ids(co_id, self.id);
                Evented::deregister(&*self.io, event_loop, token);
                self.registered = false;
            }
        }
}

/// Event source inside a coroutine
///
/// Event sources are a core of Mioco. Mioco coroutines use them to handle
/// IO in a blocking fashion.
///
/// They come in different flavours and can be created from native `mio` types by wrapping within
/// a coroutine with `MiocoHandle::wrap()` or type-specific constructors like `mailbox()` or
/// `MiocoHandle::timer()`.
#[derive(Clone)]
pub struct EventSource<T> {
    unwrapped : bool,
    inn : RcEventSourceShared,
    _t: PhantomData<T>,
}

impl<T> EventSource<T> {
    fn io(&self) -> &mut T {
        let object : TraitObject = unsafe { transmute(&*self.inn.borrow().io) };
        unsafe { transmute(object.data) }
    }
}

/// Id of an event source used to enumerate them
///
/// It's unique within coroutine of an event source, but not globally.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct EventSourceId(usize);

impl EventSourceId {
    fn as_usize(&self) -> usize {
        self.0
    }
}

impl slab::Index for EventSourceId {
    fn as_usize(&self) -> usize {
        self.0
    }
    fn from_usize(i : usize) -> Self {
        EventSourceId(i)
    }
}

/// Id of a Coroutine used to enumerate them
///
/// It's unique within a thread
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CoroutineId(usize);

impl CoroutineId {
    fn as_usize(&self) -> usize {
        self.0
    }
}

impl slab::Index for CoroutineId {
    fn as_usize(&self) -> usize {
        self.0
    }
    fn from_usize(i : usize) -> Self {
        CoroutineId(i)
    }
}

impl<T> EventSource<T>
where T : Reflect+'static {
    /// Mark the `EventSourceRef` blocked and block until `Handler` does
    /// not wake us up again.
    fn block_on(&self, rw : RW) {
        {
            let inn = self.inn.borrow();
            let mut co_shared = inn.coroutine.borrow_mut();
            trace!("Coroutine({}): blocked on {:?}", co_shared.id.as_usize(), rw);
            co_shared.state = State::BlockedOn(rw);
            // TODO: https://github.com/contain-rs/bit-vec/pulls
            co_shared.blocked_on.clear();
            co_shared.blocked_on.set(inn.id.as_usize(), true);
        };
        let co_ref = self.inn.borrow().coroutine.clone();
        coroutine_jump_out(&co_ref);
        {
            let inn = self.inn.borrow_mut();
            entry_point(&inn.coroutine);
            let co = inn.coroutine.borrow_mut();
            trace!("Coroutine({}): resumed due to event {:?}", co.id.as_usize(), co.last_event);
            debug_assert!(rw.has_read() || co.last_event.has_write());
            debug_assert!(rw.has_write() || co.last_event.has_read());
            debug_assert!(co.last_event.id().as_usize() == inn.id.as_usize());
        }
    }

    /// Access raw mio type
    pub fn with_raw<F, R>(&self, f : F) -> R
        where F : Fn(&T) -> R {
        f(self.io())
    }

    /// Access mutable raw mio type
    pub fn with_raw_mut<F, R>(&mut self, f : F) -> R
        where F : Fn(&mut T) -> R {
        f(self.io())
    }

    /// Index identificator of a `EventSource`
    pub fn id(&self) -> EventSourceId {
        EventSourceId(self.inn.borrow().id.as_usize())
    }
}

impl<T> EventSource<T>
where T : mio::TryAccept+Reflect+'static {
    /// Block on accept
    pub fn accept(&self) -> io::Result<T::Output> {
        loop {
            let res = self.io().accept();

            match res {
                Ok(None) => {
                    self.block_on(RW::read())
                },
                Ok(Some(r))  => {
                    return Ok(r);
                },
                Err(e) => {
                    return Err(e)
                }
            }
        }
    }

    /// Try accepting
    pub fn try_accept(&self) -> io::Result<Option<T::Output>> {
        self.io().accept()
    }
}

impl<T> std::io::Read for EventSource<T>
where T : TryRead+Reflect+'static {
    /// Block on read
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let res = self.io().try_read(buf);

            match res {
                Ok(None) => {
                    self.block_on(RW::read())
                },
                Ok(Some(r))  => {
                    return Ok(r);
                },
                Err(e) => {
                    return Err(e)
                }
            }
        }
    }
}

impl<T> EventSource<T>
where T : TryRead+Reflect+'static {
    /// Try to read without blocking
    pub fn try_read(&mut self, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
        self.io().try_read(buf)
    }
}

impl<T> std::io::Write for EventSource<T>
where T : TryWrite+Reflect+'static {
    /// Block on write
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        loop {
            let res = self.io().try_write(buf);

            match res {
                Ok(None) => {
                    self.block_on(RW::write())
                },
                Ok(Some(r)) => {
                    return Ok(r);
                },
                Err(e) => {
                    return Err(e)
                }
            }
        }
    }

    /// Flush. This currently does nothing
    ///
    /// TODO: Should we do something with the flush? --dpc */
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<T> EventSource<T>
where T : TryWrite+Reflect+'static {
    /// Try to write without blocking
    pub fn try_write(&mut self, buf: &[u8]) -> std::io::Result<Option<usize>> {
        self.io().try_write(buf)
    }
}

impl EventSource<UdpSocket> {
    /// Try to read without blocking
    pub fn try_read(&mut self, buf: &mut [u8]) -> std::io::Result<Option<(usize, SocketAddr)>> {
        self.io().recv_from(buf)
    }

    /// Block on read
    pub fn read(&mut self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        loop {
            let res = self.io().recv_from(buf);

            match res {
                Ok(None) => {
                    self.block_on(RW::read())
                },
                Ok(Some(r))  => {
                    return Ok(r);
                },
                Err(e) => {
                    return Err(e)
                }
            }
        }
    }

    /// Try to read without blocking
    pub fn try_write(&mut self, buf: &mut [u8], target : &SocketAddr) -> std::io::Result<Option<(usize)>> {
        self.io().send_to(buf, target)
    }

    /// Block on write
    pub fn write(&mut self, buf: &[u8], target : &SocketAddr) -> std::io::Result<(usize)> {
        loop {
            let res = self.io().send_to(buf, target);

            match res {
                Ok(None) => {
                    self.block_on(RW::write())
                },
                Ok(Some(r)) => {
                    return Ok(r);
                },
                Err(e) => {
                    return Err(e)
                }
            }
        }
    }
}

impl<T> Drop for EventSource<T> {
    fn drop(&mut self) {
        if !self.unwrapped {
            let coroutine = tl_coroutine_current();
            coroutine.unwrap_event_source(self);
        }
    }
}

fn select_impl_set_mask_from_ids(ids : &[EventSourceId], blocked_on : &mut BitVec<usize>) {
    {
        // TODO: https://github.com/contain-rs/bit-vec/pulls
        blocked_on.clear();
        for &id in ids {
            blocked_on.set(id.as_usize(), true);
        }
    }
}

fn select_impl_set_mask_rc_handles<'a>(
    handles : SlabMutIter<'a, Rc<RefCell<EventSourceShared>>, EventSourceId>, blocked_on: &mut BitVec<usize>
    ) {
    // TODO: https://github.com/contain-rs/bit-vec/pulls
    blocked_on.clear();
    for io in handles {
        blocked_on.set(io.borrow().id.as_usize(), true);
    }
}


/// Handle to spawned coroutine
pub struct CoroutineHandle {
     coroutine: RcCoroutine,
}

impl CoroutineHandle {
    /// Create an exit notificator
    pub fn exit_notificator(&self) -> MailboxInnerEnd<ExitStatus> {
        let (outer, inner) = mailbox();
        let mut co = self.coroutine.borrow_mut();
        let Coroutine {
            ref state,
            ref mut exit_notificators,
            ..
        } = *co;

        if let &State::Finished(ref exit) = state {
            outer.send(exit.clone())
        } else {
            exit_notificators.push(outer);
        }
        inner
    }
}

/// Spawn a `mioco` coroutine
///
/// If called inside an existing coroutine spawns a new coroutine. If called
/// outside of existing coroutine, it's the same as `mioco::start()`.
///
/// `f` is routine handling connection. It must not use any real blocking-IO operations, only
/// `mioco` provided types (`EventSource`) and `MiocoHandle` functions. Otherwise `mioco`
/// cooperative scheduling can block on real blocking-IO which defeats using mioco.
pub fn spawn<F>(f : F)
where F : FnOnce() -> io::Result<()> + Send + 'static {
    let coroutine = TL_CURRENT_COROUTINE.with(|coroutine| *coroutine.borrow());
    if coroutine == ptr::null_mut() {
        start(f)
    } else {
        spawn_ext(f);
    }
}

/// Spawn a `mioco` coroutine
///
/// Can't be used outside of existing coroutine.
///
/// Returns a `CoroutineHandle` that can be used to perform
/// additional operations.
// TODO: Could this be unified with `spawn()` so the return type
// can be simply ignored?
pub fn spawn_ext<F>(f : F) -> CoroutineHandle
where F : FnOnce() -> io::Result<()> + Send + 'static {
    let coroutine = tl_coroutine_current();

    let coroutine_ref = Coroutine::spawn(
        coroutine.handler_shared.as_ref().unwrap().clone(),
        f
        );
    let ret = CoroutineHandle {
        coroutine: coroutine_ref.clone(),
    };
    coroutine.children_to_start.push(coroutine_ref);

    ret
}

/// Execute a block of synchronous operations
///
/// This will execute a block of synchronous operations without blocking
/// cooperative coroutine scheduling. This is done by offloading the
/// synchronous operations to a separate thread, a notifying the
/// coroutine when the result is available.
///
/// TODO: find some wise people to confirm if this is sound
/// TODO: use threadpool to prevent potential system starvation?
pub fn sync<'b, F, R>(f : F) -> R
where F : FnMut() -> R + 'b {

    struct FakeSend<F>(F);

    unsafe impl<F> Send for FakeSend<F> { };

    let f = FakeSend(f);

    let coroutine = tl_coroutine_current();

    if coroutine.sync_mailbox.is_none() {
        let (send, recv) = mailbox();
        let recv = wrap(recv);
        coroutine.sync_mailbox = Some((send, recv));
    }

    let &(ref mail_send, ref mail_recv) = coroutine.sync_mailbox.as_ref().unwrap();
    let join = unsafe {thread_scoped::scoped(move || {
        let FakeSend(mut f) = f;
        let res = f();
        mail_send.send(());
        FakeSend(res)
    })};

    mail_recv.read();

    let FakeSend(res) = join.join();
    res
}

/// Register `mio`'s native io type to be used within `mioco` coroutine
///
/// Consumes the `io`, returns a mioco wrapper over it. Use this wrapped IO
/// to perform IO.
pub fn wrap<T : 'static>(raw_io : T) -> EventSource<T>
where T : Evented {
    let coroutine = tl_coroutine_current();

    let index = {
        let &mut Coroutine {
            ref mut io,
            ref mut registered,
            ref mut blocked_on,
            ref self_rc,
            ..
        } = coroutine;

        if !io.has_remaining() {
            let count = io.count();
            io.grow(count);
        }

        let index = io.insert_with(|index| {
            let io_new = {
                Rc::new(RefCell::new(
                        EventSourceShared {
                            coroutine : self_rc.as_ref().unwrap().clone(),
                            io: Box::new(raw_io),
                            peer_hup: false,
                            id: index,
                            registered: false,
                        }
                        ))
            };

            let index_usize = index.as_usize();
            if index_usize >= blocked_on.len() {
                blocked_on.push(false);
                registered.push(false);
            } else {
                assert_eq!(blocked_on.get(index_usize).unwrap(), false);
                assert_eq!(registered.get(index_usize).unwrap(), false);
            }

            io_new
        });
        index
    };

    EventSource {
        inn: coroutine.io[index.unwrap()].clone(),
        unwrapped : false,
        _t: PhantomData,
    }
}

/// Deregister `mio`'s native io type from owning `mioco` coroutine
///
/// Consumes the wrapped `io`, returns the original `io`.
///
/// This function is useful, when `EventSource<T>` was used in
/// one coroutine, and then needs to be moved to another.
pub fn unwrap<T : 'static>(mut event_source : EventSource<T>) -> T
where T : Evented {
    let coroutine = tl_coroutine_current();

    let io = coroutine.unwrap_event_source(&mut event_source);

    drop(event_source);

    let EventSourceShared {
        mut io,
        ..
    } = Rc::try_unwrap(io).ok().unwrap().into_inner();

    let raw_io = io.as_any_mut().downcast_mut::<T>().unwrap() as *mut T;
    mem::forget(io);
    unsafe {*Box::from_raw(raw_io) }
}

/// Get number of threads of the Mioco instance that coroutine is
/// running in.
///
/// This is useful for load balancing: spawning as many coroutines as
/// there is handling threads that can run them.
pub fn thread_num() -> usize {
    let coroutine = tl_coroutine_current();

    let handler_shared = coroutine.handler_shared.as_ref().unwrap().borrow();
    handler_shared.thread_num()
}

/// Get mutable reference to a timer source io for this coroutine
///
/// Each coroutine has one internal Timer source, that will become readable
/// when it's timeout (see `set_timer()` ) expire.
pub fn timer() -> &'static mut EventSource<Timer> {
    let coroutine = tl_coroutine_current();

    match coroutine.timer {
        Some(ref mut timer) => timer,
        None => {
            coroutine.timer = Some(wrap(Timer::new()));
            coroutine.timer.as_mut().unwrap()
        }
    }
}

// TODO: Technically this leaks unsafe, but only within
// internals of the module. Any function calling `tl_coroutine_current()`
// must not pass the reference anywhere outside!
//
// It might be possible to use a type system to enforce this. Eg. maybe this
// should return `Ref` or `RefCell`.
fn tl_coroutine_current() -> &'static mut Coroutine {
    let coroutine = TL_CURRENT_COROUTINE.with(|coroutine| *coroutine.borrow());
    if coroutine == ptr::null_mut() {
        panic!(); // TODO: Change to Err
    }
    unsafe { &mut *coroutine }
}

/// Block coroutine for a given time
///
/// Warning: The precision of this call (and other `timer()` like
/// functionality) is limited by `mio` event loop settings. Any small
/// value of `time_ms` will effectively be rounded up to
/// `mio::EventLoop::timer_tick_ms()`.
pub fn sleep(time_ms : i64) {
    let prev_timeout = timer().get_timeout_absolute();
    timer().set_timeout(time_ms);
    let _ = timer().read();
    timer().set_timeout_absolute(prev_timeout);
}

/// Wait till a read event is ready
/// TODO: move to `impl Coroutine`
fn select_impl(rw : RW) -> Event {
    let coroutine = tl_coroutine_current();
    coroutine.state = State::BlockedOn(rw);
    trace!("Coroutine({}): blocked on {:?}", coroutine.id.as_usize(), rw);
    coroutine_jump_out(&coroutine.self_rc.as_ref().unwrap());
    entry_point(&coroutine.self_rc.as_ref().unwrap());
    trace!("Coroutine({}): resumed due to event {:?}", coroutine.id.as_usize(), coroutine.last_event);
    debug_assert!(coroutine.state.is_running());
    let e = coroutine.last_event;
    e
}

/// Yield coroutine execution
///
/// Coroutine can yield execution without blocking on anything
/// particular to allow scheduler to run other coroutines before
/// resuming execution of the current one.
///
/// For this to be effective, custom scheduler must be implemented.
/// See `trait Scheduler`.
///
/// Note: named `yield_now` as `yield` is reserved word.
pub fn yield_now() {
    let coroutine = tl_coroutine_current();
    coroutine.state = State::BlockedOn(RW::none());
    trace!("Coroutine({}): yield", coroutine.id.as_usize());
    coroutine_jump_out(&coroutine.self_rc.as_ref().unwrap());
    entry_point(&coroutine.self_rc.as_ref().unwrap());
    trace!("Coroutine({}): resumed after yield ", coroutine.id.as_usize());
    debug_assert!(coroutine.state.is_running());
}

/// Wait till an event is ready
///
/// **Warning**: Mioco can't guarantee that the returned `EventSource` will
/// not block when actually attempting to `read` or `write`. You must
/// use `try_read` and `try_write` instead.
///
/// The returned value contains event type and the id of the `EventSource`.
/// See `EventSource::id()`.
pub fn select() -> Event {
    let coroutine = tl_coroutine_current();
    {
        let Coroutine {
            ref mut io,
            ref mut blocked_on,
            ..
        } = *coroutine;

        select_impl_set_mask_rc_handles(io.iter_mut(), blocked_on);
    }
    select_impl(RW::both())
}

/// Wait till a read event is ready
///
/// See `MiocoHandle::select`.
pub fn select_read() -> Event {
    let coroutine = tl_coroutine_current();
    {
        let Coroutine {
            ref mut io,
            ref mut blocked_on,
            ..
        } = *coroutine;

        select_impl_set_mask_rc_handles(io.iter_mut(), blocked_on);
    }
    select_impl(RW::read())
}

/// Wait till a read event is ready.
///
/// See `MiocoHandle::select`.
pub fn select_write() -> Event {
    let coroutine = tl_coroutine_current();
    {
        let Coroutine {
            ref mut io,
            ref mut blocked_on,
            ..
        } = *coroutine;

        select_impl_set_mask_rc_handles(io.iter_mut(), blocked_on);
    }
    select_impl(RW::write())
}

/// Wait till any event is ready on a set of Handles.
///
/// See `EventSource::id()`.
/// See `MiocoHandle::select()`.
pub fn select_from(ids : &[EventSourceId]) -> Event {
    let coroutine = tl_coroutine_current();
    {
        let Coroutine {
            ref mut blocked_on,
            ..
        } = *coroutine;

        select_impl_set_mask_from_ids(ids, blocked_on);
    }

    select_impl(RW::both())
}

/// Wait till write event is ready on a set of Handles.
///
/// See `MiocoHandle::select_from`.
pub fn select_write_from(ids : &[EventSourceId]) -> Event {
    let coroutine = tl_coroutine_current();
    {
        let Coroutine {
            ref mut blocked_on,
            ..
        } = *coroutine;

        select_impl_set_mask_from_ids(ids, blocked_on);
    }

    select_impl(RW::write())
}

/// Wait till read event is ready on a set of Handles.
///
/// See `MiocoHandle::select_from`.
pub fn select_read_from(ids : &[EventSourceId]) -> Event {
    let coroutine = tl_coroutine_current();
    {
        let Coroutine {
            ref mut blocked_on,
            ..
        } = *coroutine;

        select_impl_set_mask_from_ids(ids, blocked_on);
    }

    select_impl(RW::read())
}


struct HandlerThreadShared {
    mioco_started: AtomicUsize,
    coroutines_num : AtomicUsize,
    #[allow(dead_code)] // Not used yet
    thread_num : AtomicUsize,
}

impl HandlerThreadShared {
    fn new(thread_num : usize) -> Self {
        HandlerThreadShared {
            mioco_started: AtomicUsize::new(0),
            coroutines_num: AtomicUsize::new(0),
            thread_num: AtomicUsize::new(thread_num),
        }
    }
}

/// Data belonging to `Handler`, but referenced and manipulated by coroutinees
/// belonging to it.
struct HandlerShared {
    /// Slab allocator
    coroutines : slab::Slab<CoroutineSlabHandle, CoroutineId>,

    /// Context saved when jumping into coroutine
    context : Context,

    /// Senders to other EventLoops
    senders : Vec<MioSender>,

    /// Shared between threads
    thread_shared : ArcHandlerThreadShared,

    /// Default stack size
    stack_size : usize,

    /// Newly spawned Coroutines
    spawned : Vec<CoroutineControl>,

    /// Coroutines that were made ready
    ready : Vec<CoroutineControl>,
}

impl HandlerShared {
    fn new(senders : Vec<MioSender>, thread_shared : ArcHandlerThreadShared, stack_size : usize) -> Self {
        HandlerShared {
            coroutines: Slab::new(512),
            thread_shared: thread_shared,
            context: Context::empty(),
            senders: senders,
            stack_size: stack_size,
            spawned: Vec::new(),
            ready: Vec::new(),
        }
    }

    fn wait_for_start_all(&self) {
        while self.thread_shared.mioco_started.load(Ordering::SeqCst) == 0 {
            thread::yield_now()
        }
    }

    fn signal_start_all(&self) {
        self.thread_shared.mioco_started.store(1, Ordering::SeqCst)
    }

    fn coroutines_num(&self) -> usize {
        // Relaxed is OK, since Threads will eventually notice if it goes to
        // zero and at the start `SeqCst` in `mioco_start` and
        // `mioco_started` will enforce that `coroutines_num > 0` is visible
        // on all threads at the start.
        self.thread_shared.coroutines_num.load(Ordering::Relaxed)
    }

    fn coroutines_inc(&self) {
        self.thread_shared.coroutines_num.fetch_add(1, Ordering::SeqCst);
    }

    fn coroutines_dec(&self) {
        let prev = self.thread_shared.coroutines_num.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(prev > 0);
    }

    /// Get number of threads
    fn thread_num(&self) -> usize {
        self.thread_shared.thread_num.load(Ordering::Relaxed)
    }


}

/// Coroutine Scheduler
///
/// Custom implementations of this trait allow users to change the order in
/// which Coroutines are being scheduled.
pub trait Scheduler {
    /// Spawn per-thread Scheduler
    fn spawn_thread(&mut self) -> Box<SchedulerThread + 'static>;
}


/// Per-thread Scheduler
pub trait SchedulerThread : Send {
    /// New coroutine was spawned.
    ///
    /// This can be used to run it immediately (see
    /// `CoroutineControl::resume()`), save it to be started later, or
    /// migrate it to different thread immediately (see
    /// `CoroutineControl::migrate()`).
    ///
    /// Dropping `coroutine_ctrl` means the corresponding coroutine will be
    /// killed.
    fn spawned(&mut self, event_loop: &mut mio::EventLoop<Handler>, coroutine_ctrl: CoroutineControl);

    /// A Coroutine became ready.
    ///
    /// `coroutine_ctrl` is a control reference to the Coroutine that became
    /// ready (to be resumed). It can be resumed immediately, or stored
    /// somewhere to be resumed later.
    ///
    /// Dropping `coroutine_ctrl` means the corresponding coroutine will be
    /// killed.
    fn ready(&mut self, event_loop: &mut mio::EventLoop<Handler>, coroutine_ctrl: CoroutineControl);

    /// Mio's tick have completed.
    ///
    /// Mio signals events in batches, after which a `tick` is signaled.
    ///
    /// All events events have been processed and all unblocked coroutines
    /// signaled with `SchedulerThread::ready()`.
    ///
    /// After returning from this function, `mioco` will let mio process a
    /// new batch of events.
    fn tick(&mut self, _event_loop: &mut mio::EventLoop<Handler>) {}
}

/// Default, simple first-in-first-out Scheduler.
///
/// Newly spawned coroutines will be spread in round-robbin fashion
/// between threads.
struct FifoScheduler {
    thread_num : Arc<AtomicUsize>,
}

impl FifoScheduler {
    pub fn new() -> Self {
        FifoScheduler {
            thread_num : Arc::new(AtomicUsize::new(0)),
        }
    }
}

struct FifoSchedulerThread {
    thread_i : usize,
    thread_num : Arc<AtomicUsize>,
}

impl Scheduler for FifoScheduler {
    fn spawn_thread(&mut self) -> Box<SchedulerThread> {
        self.thread_num.fetch_add(1, Ordering::Relaxed);
        Box::new(FifoSchedulerThread{
            thread_i: 0,
            thread_num: self.thread_num.clone(),
        })
    }
}

impl FifoSchedulerThread {
    fn thread_next_i(&mut self) -> usize {
        self.thread_i += 1;
        if self.thread_i >= self.thread_num() {
            self.thread_i = 0;
        }
        self.thread_i
    }

    fn thread_num(&self) -> usize {
        self.thread_num.load(Ordering::Relaxed)
    }
}

impl SchedulerThread for FifoSchedulerThread {
    fn spawned(&mut self, event_loop: &mut mio::EventLoop<Handler>, coroutine_ctrl: CoroutineControl) {
        let thread_i = self.thread_next_i();
        trace!("Migrating newly spawn Coroutine to thread {}", thread_i);
        coroutine_ctrl.migrate(event_loop, thread_i);
    }

    fn ready(&mut self, event_loop: &mut mio::EventLoop<Handler>, coroutine_ctrl: CoroutineControl) {
        coroutine_ctrl.resume(event_loop);
    }

    fn tick(&mut self, _: &mut mio::EventLoop<Handler>) {}
}

/// Mioco event loop `Handler`
///
/// Registered in `mio::EventLoop` and implementing `mio::Handler`.  This `struct` is quite
/// internal so you should not have to worry about it.
pub struct Handler {
    shared : RcHandlerShared,
    scheduler : Box<SchedulerThread+'static>,
}

impl Handler {
    fn new(shared : RcHandlerShared, scheduler : Box<SchedulerThread>) -> Self {
        Handler {
            shared: shared,
            scheduler: scheduler,
        }
    }

    /// To prevent recursion, all the newly spawned or newly made
    /// ready Coroutines are delivered to scheduler here.
    fn deliver_to_scheduler(&mut self, event_loop : &mut EventLoop<Self>) {
        let Handler {
            ref shared,
            ref mut scheduler,
        } = *self;

        loop {
            let mut spawned = Vec::new();
            let mut ready = Vec::new();
            {
                let mut shared = shared.borrow_mut();

                if shared.spawned.len() == 0 && shared.ready.len() == 0 {
                    break;
                }
                std::mem::swap(&mut spawned, &mut shared.spawned);
                std::mem::swap(&mut ready, &mut shared.ready);
            }

            for spawned in spawned.drain(..) {
                scheduler.spawned(event_loop, spawned);
            }

            for ready in ready.drain(..) {
                scheduler.ready(event_loop, ready);
            }
        }
    }
}

/// EventLoop message type
pub enum Message {
    /// Mailbox notification
    MailboxMsg(Token),
    /// Coroutine migration
    Migration(Rc<RefCell<Coroutine>>),
}

unsafe impl Send for Message { }

impl mio::Handler for Handler {
    type Timeout = Token;
    type Message = Message;

    fn tick(&mut self, event_loop: &mut mio::EventLoop<Self>) {
        let coroutines_num = self.shared.borrow().coroutines_num();
        trace!("Handler::tick(): coroutines_num = {}", coroutines_num);
        if coroutines_num == 0 {
            trace!("Shutting down EventLoop");
            event_loop.shutdown();
        }
    }

    fn ready(&mut self, event_loop: &mut mio::EventLoop<Handler>, token: mio::Token, events: mio::EventSet) {
        trace!("Handler::ready({:?}): started", token);
        let (co_id, _) = token_to_ids(token);
        let co = {
            let shared = self.shared.borrow();
            match shared.coroutines.get(co_id).as_ref() {
                Some(&co) => co.clone(),
                None => {
                    trace!("Handler::ready() ignored");
                    return
                },
            }
        };
        if co.event(event_loop, token, events) {
            self.scheduler.ready(event_loop, co.to_coroutine_control());
        }

        self.deliver_to_scheduler(event_loop);

        trace!("Handler::ready({:?}): finished", token);
    }

    fn notify(&mut self, event_loop: &mut EventLoop<Handler>, msg: Self::Message) {
        match msg {
            MailboxMsg(token) => self.ready(event_loop, token, EventSet::readable()),
            Migration(rc_coroutine) => {
                let mut co = CoroutineControl::new(rc_coroutine);
                co.reattach_to(self);
                self.scheduler.ready(event_loop, co);
                self.deliver_to_scheduler(event_loop);
            },
        }
    }

    fn timeout(&mut self, event_loop: &mut EventLoop<Self>, msg: Self::Timeout) {
        self.ready(event_loop, msg, EventSet::readable());
    }
}

/// Mioco instance
///
/// Main mioco structure.
pub struct Mioco {
    join_handles : Vec<thread::JoinHandle<()>>,
    config : Config,
}

impl Mioco {
    /// Create new `Mioco` instance
    pub fn new() -> Self {
        Mioco::new_configured(Config::new())
    }

    /// Create new `Mioco` instance
    pub fn new_configured(config : Config) -> Self
    {
        Mioco {
            join_handles: Vec::new(),
            config: config,
         }
    }

    /// Start mioco handling
    ///
    /// Takes a starting handler function that will be executed in `mioco` environment.
    ///
    /// Will block until `mioco` is finished - there are no more handlers to run.
    ///
    /// See `MiocoHandle::spawn()`.
    pub fn start<F>(&mut self, f : F)
        where
        F : FnOnce() -> io::Result<()> + Send + 'static,
        F : Send
        {
            info!("Starting mioco instance with {} handler threads", self.config.thread_num);
            let thread_shared = Arc::new(HandlerThreadShared::new(self.config.thread_num));

            let mut event_loops = VecDeque::new();
            let mut senders = Vec::new();
            for _ in 0..self.config.thread_num {
                let event_loop = EventLoop::configured(self.config.event_loop_config.clone()).expect("new EventLoop");
                senders.push(event_loop.channel());
                event_loops.push_back(event_loop);
            }

            let sched = self.config.scheduler.spawn_thread();
            let first_event_loop = event_loops.pop_front().unwrap();

            for i in 1..self.config.thread_num {
                let sched = self.config.scheduler.spawn_thread();

                let stack_size = self.config.stack_size;
                let event_loop = event_loops.pop_front().unwrap();
                let senders = senders.clone();
                let thread_shared = thread_shared.clone();
                let join = std::thread::Builder::new().name(format!("mioco_thread_{}", i)).spawn(move || {
                    Mioco::thread_loop::<F>(None, sched, event_loop, senders, thread_shared, stack_size);
                });

                match join {
                    Ok(join) => self.join_handles.push(join),
                    Err(err) => panic!("Couldn't spawn thread: {}", err),
                }
            }

            Mioco::thread_loop(Some(f), sched, first_event_loop, senders, thread_shared, self.config.stack_size);

            for join in self.join_handles.drain(..) {
                let _ = join.join(); // TODO: Do something with it
            }
        }

    fn thread_loop<F>(
        f : Option<F>,
        mut scheduler : Box<SchedulerThread+'static>,
        mut event_loop : EventLoop<Handler>,
        senders : Vec<MioSender>,
        thread_shared : ArcHandlerThreadShared,
        stack_size: usize,
        )
        where F : FnOnce() -> io::Result<()> + Send + 'static,
              F : Send
    {
            let handler_shared = HandlerShared::new(senders, thread_shared, stack_size);
            let shared = Rc::new(RefCell::new(handler_shared));
            if let Some(f) = f {
                let coroutine_rc = Coroutine::spawn(shared.clone(), f);
                let coroutine_ctrl = CoroutineControl::new(coroutine_rc);
                scheduler.spawned(&mut event_loop, coroutine_ctrl);
                // Mark started only after first coroutine is spawned so that
                // threads don't start, detect no coroutines, and exit prematurely
                shared.borrow().signal_start_all();
            }
            let mut handler = Handler::new(shared, scheduler);

            handler.shared.borrow().wait_for_start_all();
            handler.deliver_to_scheduler(&mut event_loop);
            event_loop.run(&mut handler).unwrap();
    }
}

/// Create a Mailbox
///
/// Mailbox can be used to deliver notifications to handlers from anywhere:
///
/// * other coroutines,
/// * outside of Mioco, even a different thread.
///
pub fn mailbox<T>() -> (MailboxOuterEnd<T>, MailboxInnerEnd<T>) {
    let shared = MailboxShared {
        token: None,
        sender: None,
        inn: VecDeque::new(),
        interest: EventSet::none(),
    };

    let shared = Arc::new(Mutex::new(shared));

    (MailboxOuterEnd::new(shared.clone()), MailboxInnerEnd::new(shared))
}

type MailboxQueue<T> = Option<T>;

struct MailboxShared<T> {
    token : Option<Token>,
    sender : Option<MioSender>,
    inn : VecDeque<T>,
    interest : EventSet,
}

/// Outside Mailbox End
///
/// Use from outside the coroutine handler.
///
/// Create with `mailbox()`
pub struct MailboxOuterEnd<T> {
    shared : ArcMailboxShared<T>,
}

impl<T> Clone for MailboxOuterEnd<T> {
    fn clone(&self) -> Self {
        MailboxOuterEnd {
            shared: self.shared.clone()
        }
    }
}

/// Inner Mailbox End
///
/// Use from within coroutine handler.
///
/// Create with `mailbox()`.
pub struct MailboxInnerEnd<T> {
    shared : ArcMailboxShared<T>,
}

impl<T> MailboxOuterEnd<T> {
    fn new(shared : ArcMailboxShared<T>) -> Self {
        MailboxOuterEnd {
            shared: shared
        }
    }
}

impl<T> MailboxInnerEnd<T> {
    fn new(shared : ArcMailboxShared<T>) -> Self {
        MailboxInnerEnd {
            shared: shared
        }
    }
}

impl<T> MailboxOuterEnd<T> {
    /// Deliver `T` to the other end of the mailbox.
    ///
    /// Mailbox behaves like a queue.
    ///
    /// This is non-blocking operation.
    ///
    /// See `EventSource<MailboxInnerEnd<T>>::read()`.
    pub fn send(&self, t : T) {
        let mut lock = self.shared.lock();
        let MailboxShared {
            ref mut sender,
            ref mut token,
            ref mut inn,
            ref mut interest,
        } = *lock;

        inn.push_back(t);
        debug_assert!(!inn.is_empty());
        trace!("MailboxOuterEnd: putting message in a queue; new len: {}", inn.len());

        if interest.is_readable() {
            let token = token.unwrap();
            trace!("MailboxOuterEnd: notifying {:?}", token);
            let sender = sender.as_ref().unwrap();
            sender_retry(&sender, MailboxMsg(token))
        }
    }
}

impl<T> EventSource<MailboxInnerEnd<T>>
where T : Reflect+'static {
    /// Receive `T` sent using corresponding `MailboxOuterEnd::send()`.
    ///
    /// Will block coroutine if no elements are available.
    pub fn read(&self) -> T {
        loop {
            if let Some(t) = self.try_read() {
                return t
            }

            self.block_on(RW::read())
        }
    }

    /// Try reading current time (if the timer is done)
    pub fn try_read(&self) -> Option<T> {
        let mut inn = self.inn.borrow_mut();
        let handle = inn.io.as_any_mut().downcast_mut::<MailboxInnerEnd<T>>().unwrap();
        let mut lock = handle.shared.lock();

        lock.inn.pop_front()
    }
}

/// A Timer generating event after a given time
///
/// Can be used to block coroutine or to implement timeout for other `EventSource`.
///
/// Create using `MiocoHandle::timeout()`.
///
/// Use `MiocoHandle::select()` to wait for an event, or `read()` to block until
/// done.
pub struct Timer {
    timeout: SteadyTime,
}

impl Timer {
    fn new() -> Timer {
        Timer { timeout: SteadyTime::now() }
    }

    fn is_done(&self) -> bool {
        self.timeout <= SteadyTime::now()
    }
}

impl EventSource<Timer> {
    /// Read a timer to block on it until it is done.
    ///
    /// Returns current time
    ///
    /// TODO: Return wakeup time instead
    pub fn read(&mut self) -> SteadyTime {
        loop {
            if let Some(t) = self.try_read() {
                return t;
            }

            self.block_on(RW::read());
        }
    }

    /// Try reading current time (if the timer is done)
    ///
    /// TODO: Return wakeup time instead
    pub fn try_read(&mut self) -> Option<SteadyTime> {
        let done = self.with_raw(|timer| { timer.is_done() });

        if done {
            Some(SteadyTime::now())
        } else {
            None
        }
    }

    /// Set timeout for the timer
    ///
    /// The timeout counts from the time `set_timer` is called.
    pub fn set_timeout(&mut self, delay_ms : i64) {
        self.with_raw_mut(
            |timer|
            timer.timeout = SteadyTime::now() + Duration::milliseconds(delay_ms)
            );
    }

    fn set_timeout_absolute(&mut self, timeout : SteadyTime) {
        self.with_raw_mut(
            |timer| timer.timeout = timeout
            );
    }


    fn get_timeout_absolute(&mut self) -> SteadyTime {
        self.with_raw_mut(
            |timer| timer.timeout
            )
    }
}


/// Shorthand for creating new `Mioco` instance and starting it right away.
pub fn start<F>(f : F)
    where F : FnOnce() -> io::Result<()> + Send + 'static,
          F : Send
{
    Mioco::new().start(f);
}

/// Shorthand for creating new `Mioco` instance with a fixed number of
/// threads and starting it right away.
pub fn start_threads<F>(thread_num : usize, f : F)
    where F : FnOnce() -> io::Result<()> + Send + 'static,
          F : Send
{
    let mut config = Config::new();
    config.set_thread_num(thread_num);
    Mioco::new_configured(config).start(f);
}

/// Mioco builder
pub struct Config {
    thread_num : usize,
    scheduler : Box<Scheduler + 'static>,
    event_loop_config : EventLoopConfig,
    stack_size : usize,
}

impl Config {
    /// Create mioco `Config`
    ///
    /// Use it to configure mioco instance
    ///
    /// See `start` and `start_threads` for convenience wrappers.
    pub fn new() -> Self {
        Config {
            thread_num: num_cpus::get(),
            scheduler: Box::new(FifoScheduler::new()),
            event_loop_config: Default::default(),
            stack_size: 2 * 1024 * 1024,
        }
    }

    /// Set numer of threads to run mioco with
    ///
    /// Default is equal to a numer of CPUs in the system.
    pub fn set_thread_num(&mut self, thread_num : usize) -> &mut Self {
        self.thread_num = thread_num;
        self
    }

    /// Set custom scheduler.
    ///
    /// See `Scheduler` trait.
    ///
    /// Default is a simple FIFO-scheduler that spreads all the new
    /// coroutines between all threads in round-robin fashion, and runs them
    /// in FIFO manner.
    ///
    /// See private `FifoSchedule` source for details.
    pub fn set_scheduler(&mut self, scheduler : Box<Scheduler + 'static>) -> &mut Self {
        self.scheduler = scheduler;
        self
    }

    /// Set stack size in bytes.
    ///
    /// Default is 2MB.
    ///
    /// Should be a power of 2.
    ///
    /// Stack size includes a protection page. Setting too small stack will
    /// lead to SEGFAULTs. See [context-rs stack.rs](https://github.com/zonyitoo/context-rs/blob/master/src/stack.rs)
    /// for implementation details. The sane minimum seems to be 128KiB,
    /// which is two 64KB pages.
    pub unsafe fn set_stack_size(&mut self, stack_size : usize) -> &mut Self {
        self.stack_size = stack_size;
        self
    }

    /// Configure `mio::EvenLoop` for all the threads
    pub fn even_loop(&mut self) -> &mut EventLoopConfig {
        &mut self.event_loop_config
    }
}

#[cfg(test)]
mod tests;
