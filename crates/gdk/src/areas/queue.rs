//! The task-queue provider's object: XAsync and XTaskQueue (`taskqueue`, `xasync`). Its interface
//! id is the provider's. Slot offsets from the fork's design (`victory-hle` `xasync.rs`).

use std::ffi::c_void;

use super::{slot, This};
use crate::guid::Guid;
use crate::hr::*;
use crate::object::InterfaceDef;
use crate::taskqueue::{self as tq, Callback, Handle, MonitorCallback, PortHandle, TerminatedCallback};
use crate::xasync::{self, Provider, Work, XAsyncBlock};

pub const PROVIDER: Guid = Guid::parse("073b7dcb-1fcf-4030-94be-e3c9eb623428");

unsafe extern "system" fn get_status(_: This, b: *mut XAsyncBlock, wait: u8) -> HRESULT {
    unsafe { xasync::get_status(b, wait != 0) }
}
unsafe extern "system" fn get_result_size(_: This, b: *mut XAsyncBlock, out: *mut usize) -> HRESULT {
    unsafe { xasync::get_result_size(b, out) }
}
unsafe extern "system" fn cancel(_: This, b: *mut XAsyncBlock) {
    unsafe { xasync::cancel(b) }
}
unsafe extern "system" fn run(_: This, b: *mut XAsyncBlock, work: Option<Work>) -> HRESULT {
    unsafe { xasync::run(b, work) }
}
unsafe extern "system" fn begin(_: This, b: *mut XAsyncBlock, context: *mut c_void, identity: *const c_void, _name: *const u8, provider: Option<Provider>) -> HRESULT {
    unsafe { xasync::begin(b, context, identity, provider) }
}
unsafe extern "system" fn schedule(_: This, b: *mut XAsyncBlock, delay: u32) -> HRESULT {
    unsafe { xasync::schedule(b, delay) }
}
unsafe extern "system" fn complete(_: This, b: *mut XAsyncBlock, hr: HRESULT, size: usize) {
    unsafe { xasync::complete(b, hr, size) }
}
unsafe extern "system" fn get_result(_: This, b: *mut XAsyncBlock, identity: *const c_void, size: usize, buffer: *mut c_void, used: *mut usize) -> HRESULT {
    unsafe { xasync::get_result(b, identity, size, buffer, used) }
}
unsafe extern "system" fn create(_: This, work: u32, completion: u32, out: *mut Handle) -> HRESULT {
    unsafe { tq::create(work, completion, out) }
}
unsafe extern "system" fn create_composite(_: This, work: PortHandle, completion: PortHandle, out: *mut Handle) -> HRESULT {
    unsafe { tq::create_composite(work, completion, out) }
}
unsafe extern "system" fn get_port(_: This, q: Handle, port: u32, out: *mut PortHandle) -> HRESULT {
    unsafe { tq::get_port(q, port, out) }
}
unsafe extern "system" fn duplicate(_: This, q: Handle, out: *mut Handle) -> HRESULT {
    unsafe { tq::duplicate(q, out) }
}
unsafe extern "system" fn dispatch(_: This, q: Handle, port: u32, timeout: u32) -> bool {
    unsafe { tq::dispatch(q, port, timeout) }
}
unsafe extern "system" fn close(_: This, q: Handle) {
    unsafe { tq::close(q) }
}
unsafe extern "system" fn submit(_: This, q: Handle, port: u32, context: *mut c_void, callback: Option<Callback>) -> HRESULT {
    unsafe { tq::submit_delayed(q, port, 0, context, callback) }
}
unsafe extern "system" fn submit_delayed(_: This, q: Handle, port: u32, delay: u32, context: *mut c_void, callback: Option<Callback>) -> HRESULT {
    unsafe { tq::submit_delayed(q, port, delay, context, callback) }
}
unsafe extern "system" fn terminate(_: This, q: Handle, wait: u8, context: *mut c_void, callback: Option<TerminatedCallback>) -> HRESULT {
    unsafe { tq::terminate(q, wait != 0, context, callback) }
}
unsafe extern "system" fn register_monitor(_: This, q: Handle, context: *mut c_void, callback: Option<MonitorCallback>, token: *mut u64) -> HRESULT {
    unsafe { tq::register_monitor(q, context, callback, token) }
}
unsafe extern "system" fn unregister_monitor(_: This, q: Handle, token: u64) {
    unsafe { tq::unregister_monitor(q, token) }
}
unsafe extern "system" fn get_process_queue(_: This, out: *mut Handle) -> bool {
    unsafe { tq::get_process_queue(out) }
}
unsafe extern "system" fn set_process_queue(_: This, q: Handle) {
    unsafe { tq::set_process_queue(q) }
}
/// The fork reads this as a debug check that the caller is not a time-sensitive thread, made before
/// runtime calls; nothing to check here.
unsafe extern "system" fn check_thread(_: This) {}

pub static QUEUE: InterfaceDef = InterfaceDef {
    area: "task queue",
    provider: PROVIDER,
    iids: &[PROVIDER],
    len: 31,
    slots: &[
        slot!(0x18, "XAsyncGetStatus", get_status, "block, wait: bool -> HRESULT"),
        slot!(0x20, "XAsyncGetResultSize", get_result_size, "block, *size -> HRESULT"),
        slot!(0x28, "XAsyncCancel", cancel, "block"),
        slot!(0x30, "XAsyncRun", run, "block, work -> HRESULT"),
        slot!(0x38, "XAsyncBegin", begin, "block, context, identity, identityName: *const char, provider -> HRESULT"),
        slot!(0x48, "XAsyncSchedule", schedule, "block, delayMs: u32 -> HRESULT"),
        slot!(0x50, "XAsyncComplete", complete, "block, result: HRESULT, requiredBufferSize: usize"),
        slot!(0x58, "XAsyncGetResult", get_result, "block, identity, bufferSize, *buffer, *bufferUsed -> HRESULT"),
        slot!(0x60, "XTaskQueueCreate", create, "workMode: u32, completionMode: u32, *queue -> HRESULT"),
        slot!(0x68, "XTaskQueueCreateComposite", create_composite, "workPort, completionPort, *queue -> HRESULT"),
        slot!(0x70, "XTaskQueueGetPort", get_port, "queue, port: u32, *portHandle -> HRESULT"),
        slot!(0x78, "XTaskQueueDuplicateHandle", duplicate, "queue, *queue -> HRESULT"),
        slot!(0x80, "XTaskQueueDispatch", dispatch, "queue, port: u32, timeoutMs: u32 -> bool"),
        slot!(0x88, "XTaskQueueCloseHandle", close, "queue"),
        slot!(0x90, "XTaskQueueSubmitCallback", submit, "queue, port: u32, context, callback -> HRESULT"),
        slot!(0x98, "XTaskQueueSubmitDelayedCallback", submit_delayed, "queue, port: u32, delayMs: u32, context, callback -> HRESULT"),
        slot!(0xb0, "XTaskQueueTerminate", terminate, "queue, wait: bool, context, callback -> HRESULT"),
        slot!(0xb8, "XTaskQueueRegisterMonitor", register_monitor, "queue, context, callback, *token: u64 -> HRESULT"),
        slot!(0xc0, "XTaskQueueUnregisterMonitor", unregister_monitor, "queue, token: u64"),
        slot!(0xc8, "XTaskQueueGetCurrentProcessTaskQueue", get_process_queue, "*queue -> bool"),
        slot!(0xd0, "XTaskQueueSetCurrentProcessTaskQueue", set_process_queue, "queue"),
        slot!(0xe8, "(thread check)", check_thread, "()"),
    ],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::areas::test::{method, this};
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn through_the_object_as_the_title_calls_it() {
        type Create = unsafe extern "system" fn(This, u32, u32, *mut Handle) -> HRESULT;
        type Submit = unsafe extern "system" fn(This, Handle, u32, *mut c_void, Callback) -> HRESULT;
        type Dispatch = unsafe extern "system" fn(This, Handle, u32, u32) -> bool;
        type Close = unsafe extern "system" fn(This, Handle);
        type Check = unsafe extern "system" fn(This);
        static RAN: AtomicU32 = AtomicU32::new(0);
        unsafe extern "system" fn cb(c: *mut c_void, _: bool) {
            RAN.fetch_add(c as u32, Ordering::SeqCst);
        }
        let t = this(&QUEUE);
        let mut q = std::ptr::null();
        unsafe {
            method::<Check>(&QUEUE, 0xe8)(t);
            assert_eq!(method::<Create>(&QUEUE, 0x60)(t, 0, 0, &mut q), S_OK);
            assert_eq!(method::<Submit>(&QUEUE, 0x90)(t, q, 1, 5 as *mut c_void, cb), S_OK);
            assert!(!method::<Dispatch>(&QUEUE, 0x80)(t, q, 0, 0));
            assert!(method::<Dispatch>(&QUEUE, 0x80)(t, q, 1, 0));
            method::<Close>(&QUEUE, 0x88)(t, q);
        }
        assert_eq!(RAN.load(Ordering::SeqCst), 5);
    }
}
