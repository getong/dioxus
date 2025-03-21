use std::{
    any::TypeId,
    collections::HashMap,
    ffi::CStr,
    os::raw::c_void,
    panic::{panic_any, AssertUnwindSafe, UnwindSafe},
    path::PathBuf,
    sync::Arc,
};

pub use subsecond_macro::hot;
pub use subsecond_types::JumpTable;

mod android;
mod macho;
mod unix;
mod wasm;
mod windows;

pub mod prelude {
    pub use subsecond_macro::hot;
}

mod fn_impl;
use fn_impl::*;

#[no_mangle]
pub extern "C" fn aslr_reference() -> u64 {
    aslr_reference as *const () as u64
}

// todo: if there's a reference held while we run our patch, this gets invalidated. should probably
// be a pointer to a jump table instead, behind a cell or something. I believe Atomic + relaxed is basically a no-op
static mut APP_JUMP_TABLE: Option<JumpTable> = None;
static mut HOTRELOAD_HANDLERS: Vec<Arc<dyn Fn()>> = vec![];
static mut CHANGED: bool = false;
static mut SUBSECOND_ENABLED: bool = false;

/// Call a given function with hot-reloading enabled. If the function's code changes, `call` will use
/// the new version of the function. If code *above* the function changes, this will emit a panic
/// that forces an unwind to the next `Subsecond::call` instance.
///
/// # Example
///
///
/// # Without unwinding
///
///
/// # WebAssembly
///
/// WASM/rust does not support unwinding, so `Subsecond::call` will not track dependency graph changes.
/// If you are building a framework for use on WASM, you will need to use `Subsecond::HotFn` directly.
///
/// However, if you wrap your calling code in a future, you *can* simply drop the future which will
/// cause `drop` to execute and get something similar to unwinding. Not great if refcells are open.
pub fn call<O>(f: impl FnMut() -> O) -> O {
    let mut hotfn = current(f);

    loop {
        let res = std::panic::catch_unwind(AssertUnwindSafe(|| hotfn.call(())));

        // If the call succeeds just return the result, otherwise we try to handle the panic if its our own.
        let err = match res {
            Ok(res) => return res,
            Err(err) => err,
        };

        // If this is our panic then let's handle it, otherwise we just resume unwinding
        let Some(hot_payload) = err.downcast_ref::<HotFnPanic>() else {
            std::panic::resume_unwind(err);
        };

        // If we're not manually unwinding, then it's their panic
        // We issue a sigstop to the process so it can be debugged
        unsafe {
            if SUBSECOND_ENABLED {
                // todo: wait for the new patch to be applied
                continue;
            }
        }
    }
}

pub const fn current<A, M, F>(f: F) -> HotFn<A, M, F>
where
    F: HotFunction<A, M>,
{
    HotFn {
        inner: f,
        _marker: std::marker::PhantomData,
    }
}

pub struct HotFnPanic {}

pub struct HotFn<A, M, T: HotFunction<A, M>> {
    inner: T,
    _marker: std::marker::PhantomData<(A, M)>,
}

impl<A, M, T: HotFunction<A, M>> HotFn<A, M, T> {
    pub fn call(&mut self, args: A) -> T::Return {
        // If we need to unwind, then let's throw a panic
        // This will occur when the pending patch is "over our head" and needs to be applied to a
        // "resume point". We can eventually look into migrating the datastructures over but for now
        // the resume point will force the struct to be re-built.
        // panic_any()

        unsafe {
            // Try to handle known function pointers. This is *really really* unsafe, but due to how
            // rust trait objects work, it's impossible to make an arbitrary usize-sized type implement Fn()
            // since that would require a vtable pointer, pushing out the bounds of the pointer size.
            if size_of::<T>() == size_of::<fn() -> ()>() {
                return self.inner.call_as_ptr(args);
            }

            // Handle trait objects. This will occur for sizes other than usize. Normal rust functions
            // become ZST's and thus their <T as SomeFn>::call becomes a function pointer to the function.
            //
            // For non-zst (trait object) types, then there might be an issue. The real call function
            // will likely end up in the vtable and will never be hot-reloaded since signature takes self.
            if let Some(jump_table) = APP_JUMP_TABLE.as_ref() {
                let known_fn_ptr = <T as HotFunction<A, M>>::call_it as *const ();
                let canonical_addr = known_fn_ptr as u64;
                // let canonical_addr = known_fn_ptr as u64 & 0x00FFFFFFFFFFFFFF;
                // let canonical_addr = known_fn_ptr as u64 & 0x00FFFFFFFFFFFFFF;
                if let Some(ptr) = jump_table.map.get(&canonical_addr).cloned() {
                    // let tag = known_fn_ptr as u64 & 0xFF00000000000000;
                    let ptr = ptr as *const ();
                    let true_fn = std::mem::transmute::<*const (), fn(&T, A) -> T::Return>(ptr);
                    return true_fn(&self.inner, args);
                } else {
                    println!("Could not find detour for {:#x}", canonical_addr);
                }
            }

            self.inner.call_it(args)
        }
    }
}

pub fn register_handler(handler: Arc<dyn Fn() + Send + Sync + 'static>) {
    unsafe {
        HOTRELOAD_HANDLERS.push(handler);
    }
}

pub fn changed() -> bool {
    let changed = unsafe { CHANGED };
    unsafe { CHANGED = false };
    changed
}

/// Apply the patch using the jump table.
///
/// # Safety
///
/// This function is unsafe because it is detouring existing functions in memory. This is wildly unsafe,
/// especially if the JumpTable is malformed. Only run this if you know what you're doing.
pub unsafe fn run_patch(jump_table: JumpTable) {
    // On non-wasm platforms we can just use libloading and the known aslr offsets to load the library
    #[cfg(any(unix, windows))]
    let jump_table = relocate_native_jump_table(jump_table);

    // On wasm we need to do a lot more work - merging our ifunc table, etc
    #[cfg(target_arch = "wasm32")]
    let jump_table = relocate_wasm_jump_table(jump_table);

    // Update runtime state
    unsafe {
        APP_JUMP_TABLE = Some(jump_table);
        CHANGED = true;
        HOTRELOAD_HANDLERS.clone().iter().for_each(|handler| {
            handler();
        });
    }
}

#[cfg(any(unix, windows))]
fn relocate_native_jump_table(mut jump_table: JumpTable) -> JumpTable {
    // let old_offset = aslr_reference() - jump_table.aslr_reference;

    let old_offset = alsr_offset(
        jump_table.old_base_address as usize,
        #[cfg(unix)]
        libloading::os::unix::Library::this(),
        #[cfg(windows)]
        libloading::os::windows::Library::this().unwrap(),
    )
    .unwrap();

    let new_offset = alsr_offset(
        jump_table.new_base_address as usize,
        #[cfg(unix)]
        unsafe { libloading::os::unix::Library::new(&jump_table.lib).unwrap() }.into(),
        #[cfg(windows)]
        unsafe { libloading::Library::new(&jump_table.lib).unwrap() }.into(),
    )
    .unwrap();

    println!("known reference: {:#x}", aslr_reference());
    println!("jump orig base: {:#x}", jump_table.old_base_address);
    println!("jump new base: {:#x}", jump_table.new_base_address);
    println!("jump orig offset: {:?}", old_offset);
    println!("jump new offset: {:?}", new_offset);

    // 487557233524

    // Modify the jump table to be relative to the base address of the loaded library
    jump_table.map = jump_table
        .map
        .iter()
        .map(|(k, v)| {
            (
                (*k + old_offset as u64) as u64,
                (*v + new_offset as u64) as u64,
            )
        })
        .collect();

    println!("adjusted jump_table: {jump_table:#?}");

    jump_table
}

/// Get the offset of the current executable in the address space of the current process.
///
/// Forgets the library to prevent its drop from being calleds
fn alsr_offset(
    base_address: usize,
    #[cfg(unix)] lib: libloading::os::unix::Library,
    #[cfg(windows)] lib: libloading::os::windows::Library,
) -> Option<*mut c_void> {
    #[allow(unused_assignments)]
    let mut offset = None;

    // the only "known global symbol" for everything we compile is __rust_alloc
    // however some languages won't have this. we could consider linking in a known symbol but this works for now
    // #[cfg(any(target_os = "macos", target_os = "ios"))]
    unsafe {
        offset = lib
            .get::<*const ()>(b"__rust_alloc")
            .ok()
            .map(|ptr| ptr.as_raw_ptr());
    };

    println!("-aslr calc offset: {offset:?}");
    println!("-aslr calc base_address: {base_address:?}");

    // attempt to determine the aslr slide by using the on-disk rust-alloc symbol
    // offset.map(|offset| offset.wrapping_byte_sub(base_address as usize))
    offset.map(|offset| offset.wrapping_byte_sub(base_address))

    // #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    // unsafe {
    //     // used to be __executable_start by that doesn't work for shared libraries
    //     offset = lib
    //         .get::<*const ()>(b"__rust_alloc")
    //         .ok()
    //         .map(|ptr| ptr.as_raw_ptr());
    // };

    // Leak the library to prevent its drop from being called and unloading the library
    // let _handle = lib.into_raw() as *mut c_void;

    // // windows needs the raw handle directly to lookup the base address
    // #[cfg(windows)]
    // unsafe {
    //     offset = windows::get_module_base_address(_handle);
    // }

    // let offset = offset.unwrap() as usize;
    // // strip the tag
    // //
    // let offset = offset & 0x00FFFFFFFFFFFFFF;
    // // let offset = offset & 0x00FFFFFFFFFFFFFF;

    // // println!("offset: {offset:?}");
    // // println!("base_address: {base_address:?}");
    // // println!("base_address: {base_address:x?}");

    // let res = offset - base_address as usize;
    // // let res = offset.map(|offset| offset.wrapping_byte_sub(base_address as usize));
    // println!("res: {res:?}");
    // Some(res as _)
}

#[cfg(target_arch = "wasm32")]
fn relocate_wasm_jump_table(jump_table: JumpTable) -> JumpTable {
    todo!()
}

// 03-21 01:39:54.474 24535 24566 I RustStdoutStderr: -aslr calc offset: Some(0x71ff1ef834)
// 03-21 01:39:54.474 24535 24566 I RustStdoutStderr: -aslr calc base_address: 354356
// 03-21 01:39:54.474 24535 24566 I RustStdoutStderr: known reference: 0x719fef5078
// 03-21 01:39:54.474 24535 24566 I RustStdoutStderr: jump orig base: 0x114766c
// 03-21 01:39:54.474 24535 24566 I RustStdoutStderr: jump new base: 0x56834
// 03-21 01:39:54.474 24535 24566 I RustStdoutStderr: jump orig offset: 487982350336 (0x719E03C000)
// 03-21 01:39:54.474 24535 24566 I RustStdoutStderr: jump new offset:                0x71ff199000

// 0x71874444b0 - 17877148
//
// 0x7187 4444 b0
// 0x1787 7148 b0
//
// seems to be flipped and tagged

//
// 487987849236 aslr reference
// 487973692928 looking for
//
//
// calculated aslr offset 0x719c546000

// 01:16:49 [dev] Setting aslr_reference: 487569719956 -> 0x71856B8294
//
// 01:16:56 [dev] aslr_offset: 0x7183801000
//
// 0x71856B8294
// 0x7183801000
//
// 03-21 01:16:54.842 23835 23860 I RustStdoutStderr:         18092028: 360016,
// 03-21 01:16:54.842 23835 23860 I RustStdoutStderr:         19054588: 377400,
// 03-21 01:16:54.842 23835 23860 I RustStdoutStderr:     },
// 03-21 01:16:54.842 23835 23860 I RustStdoutStderr:     old_base_address: 18107204,
// 03-21 01:16:54.842 23835 23860 I RustStdoutStderr:     new_base_address: 352976,
// 03-21 01:16:54.842 23835 23860 I RustStdoutStderr: }
// 03-21 01:16:54.843 23835 23860 I RustStdoutStderr: Could not find detour for.. 487569422812
// 03-21 01:16:54.843 23835 23860 I RustStdoutStderr: Could not find detour for.. 487569674160
// 03-21 01:16:54.843 23835 23860 I RustStdoutStderr: Could not find detour for   487555413852
// 03-21 01:16:54.846 23835 23860 I RustStdoutStderr: Could not find detour for.. 487555557228
// 03-21 01:16:54.846 23835 23860 I RustStdoutStderr: Could not find detour for.. 487555557228
//
// 03-21 01:19:28.613 23996 24020 I RustStdoutStderr: Could not find detour for 0x7186fa2514
// 03-21 01:19:28.613 23996 24020 I RustStdoutStderr: Could not find detour for 0x7186fdfec0
// 03-21 01:19:28.613 23996 24020 I RustStdoutStderr: Could not find detour for 0x7186245cf0
// 03-21 01:19:28.614 23996 24020 I RustStdoutStderr: Could not find detour for 0x7186268fac
// 03-21 01:19:28.614 23996 24020 I RustStdoutStderr: Could not find detour for 0x7186268fac
//
// 03-21 01:19:28.608 23996 24020 I RustStdoutStderr: known reference: 0x7186feb118 (in the program)
// on disk the aslr reference is 0000000001eb8118
//
// locations of rust_alloc shift
//
// aslr slide is 0x7185133000a
//
// addr if symbol looking for is 0x1135FAC (or 18046892) which is very similar to 18092028
//
// 03-21 01:19:28.608 23996 24020 I RustStdoutStderr: jump orig base: 0x11452f8  (base of __rust_alloc on disk)
// 03-21 01:19:28.608 23996 24020 I RustStdoutStderr: jump new base: 0x567f0  (base of __rust_alloc in patch)
//
//
// 03-21 01:19:28.608 23996 24020 I RustStdoutStderr: jump orig offset: 0x74c3950fa4
// 03-21 01:19:28.608 23996 24020 I RustStdoutStderr: jump new offset: 0x71ff194000
