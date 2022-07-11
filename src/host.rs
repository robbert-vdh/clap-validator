//! Data structures and utilities for hosting plugins.

use anyhow::Result;
use clap_sys::ext::audio_ports::{clap_host_audio_ports, CLAP_EXT_AUDIO_PORTS};
use clap_sys::ext::note_ports::{
    clap_host_note_ports, clap_note_dialect, CLAP_EXT_NOTE_PORTS, CLAP_NOTE_DIALECT_CLAP,
    CLAP_NOTE_DIALECT_MIDI, CLAP_NOTE_DIALECT_MIDI_MPE,
};
use clap_sys::ext::params::{
    clap_host_params, clap_param_clear_flags, clap_param_rescan_flags, CLAP_EXT_PARAMS,
};
use clap_sys::ext::state::{clap_host_state, CLAP_EXT_STATE};
use clap_sys::ext::thread_check::{clap_host_thread_check, CLAP_EXT_THREAD_CHECK};
use clap_sys::host::clap_host;
use clap_sys::id::clap_id;
use clap_sys::version::CLAP_VERSION;
use crossbeam::atomic::AtomicCell;
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::ffi::{c_void, CStr};
use std::os::raw::c_char;
use std::pin::Pin;
use std::sync::Arc;
use std::thread::ThreadId;

use crate::plugin::instance::PluginHandle;
use crate::util::check_null_ptr;

/// An abstraction for a CLAP plugin host. It handles callback requests made by the plugin, and it
/// checks whether the calling thread matches up when any of its functions are called by the plugin.
/// A `Result` indicating the first failure, of any, can be retrieved by calling the
/// [`thread_safety_check()`][Self::thread_safety_check()] method.
///
/// Multiple plugins can share this host instance. Because of that, we can't just cast the `*const
/// clap_host` directly to a `*const ClapHost`, as that would make it impossible to figure out which
/// `*const clap_host` belongs to which plugin instance. Instead, every registered plugin instance
/// gets their own `HostPluginInstance` which provides a `clap_host` struct unique to that plugin
/// instance. This can be linked back to both the plugin instance and the shared `ClapHost`.
#[derive(Debug)]
pub struct ClapHost {
    /// The ID of the main thread.
    main_thread_id: ThreadId,
    /// A description of the first thread safety error encountered by this `ClapHost`, if any. This
    /// is used to check that the plugin called any host callbacks from the correct thread after the
    /// test has succeeded.
    thread_safety_error: Mutex<Option<String>>,

    /// These are the plugin instances taht were registered on this host. They're added here when
    /// the `Plugin` object is created, and they're removed when the object is dropped. This is used
    /// to keep track of audio threads and pending callbacks.
    instances: RwLock<HashMap<PluginHandle, Pin<Arc<HostPluginInstance>>>>,

    // These are the vtables for the extensions supported by the host
    clap_host_audio_ports: clap_host_audio_ports,
    clap_host_note_ports: clap_host_note_ports,
    clap_host_params: clap_host_params,
    clap_host_state: clap_host_state,
    clap_host_thread_check: clap_host_thread_check,
}

/// Runtime information about a plugin instance. This keeps track of pending callbacks and things
/// like audio threads.
#[derive(Debug)]
#[repr(C)]
pub struct HostPluginInstance {
    /// The vtable that's passed to the plugin. The `host_data` field is populated with a pointer to
    /// the
    clap_host: clap_host,
    /// The host this `HostPluginInstance` belongs to. This is needed to get back to the `ClapHost`
    /// instance from a `*const clap_host`, which we can cast to this struct to access the pointer.
    pub host: Arc<ClapHost>,
    /// The plugin this `HostPluginInstance` is associated with. This is the same as they key in the
    /// `ClapHost::instances` hash map, but it also needs to be stored here to make it possible to
    /// know what plugin instance a `*const clap_host` refers to.
    ///
    /// This is an `Option` because the plugin handle is only known after the plugin has been
    /// created, and the factory's `create_plugin()` function requires a pointer to the `clap_host`.
    pub plugin: AtomicCell<Option<PluginHandle>>,

    /// The plugin instance's audio thread, if it has one. Used for the audio thread checks.
    pub audio_thread: AtomicCell<Option<ThreadId>>,
}

impl HostPluginInstance {
    /// Construct a new plugin instance object. The [`HostPluginInstance::plugin`] field must be set
    /// later because the `clap_host` struct needs to be passed to `clap_factory::create_plugin()`,
    /// and the plugin instance pointer is only known after that point. This contains the
    /// `clap_host` vtable for this plugin instance, and keeps track of things like the instance's
    /// audio thread and pending callbacks. The `Pin` is necessary to prevent moving the object out
    /// of the `Box`, since that would break pointers to the `HostPluginInstance`.
    pub fn new(host: Arc<ClapHost>) -> Pin<Arc<Self>> {
        Arc::pin(Self {
            clap_host: clap_host {
                clap_version: CLAP_VERSION,
                // We can directly cast the `*const clap_host` to a `*const HostPluginInstance` because
                // it's stored as the first field of a pinned `#[repr(C)]` struct
                host_data: std::ptr::null_mut(),
                name: b"clap-validator\0".as_ptr() as *const c_char,
                vendor: b"Robbert van der Helm\0".as_ptr() as *const c_char,
                url: b"https://github.com/robbert-vdh/clap-validator\0".as_ptr() as *const c_char,
                version: b"0.1.0\0".as_ptr() as *const c_char,
                get_extension: Some(ClapHost::get_extension),
                request_restart: Some(ClapHost::request_restart),
                request_process: Some(ClapHost::request_process),
                request_callback: Some(ClapHost::request_callback),
            },
            host,
            plugin: AtomicCell::new(None),

            audio_thread: AtomicCell::new(None),
        })
    }

    /// Get the `HostPluginInstance` and the host from a valid `clap_host` pointer.
    pub unsafe fn from_clap_host_ptr<'a>(
        ptr: *const clap_host,
    ) -> (&'a HostPluginInstance, &'a ClapHost) {
        let this = &*(ptr as *const Self);
        (this, &*this.host)
    }

    /// Get a pointer to the `clap_host` struct for this instance. This uniquely identifies the
    /// instance.
    pub fn as_ptr(self: &Pin<Arc<HostPluginInstance>>) -> *const clap_host {
        // The value will not move, since this `ClapHost` can only be constructed as a
        // `Pin<Arc<HostPluginInstance>>`
        &self.clap_host
    }
}

impl ClapHost {
    /// Initialize a CLAP host. The thread this object is created on will be designated as the main
    /// thread for the purposes of the thread safety checks.
    pub fn new() -> Arc<ClapHost> {
        Arc::new(ClapHost {
            main_thread_id: std::thread::current().id(),
            // If the plugin never makes callbacks from the wrong thread, then this will remain an
            // `None`. Otherwise this will be replaced by the first error.
            thread_safety_error: Mutex::new(None),

            instances: RwLock::new(HashMap::new()),

            clap_host_audio_ports: clap_host_audio_ports {
                is_rescan_flag_supported: Some(Self::ext_audio_ports_is_rescan_flag_supported),
                rescan: Some(Self::ext_audio_ports_rescan),
            },
            clap_host_note_ports: clap_host_note_ports {
                supported_dialects: Some(Self::ext_note_ports_supported_dialects),
                rescan: Some(Self::ext_note_ports_rescan),
            },
            clap_host_params: clap_host_params {
                rescan: Some(Self::ext_params_rescan),
                clear: Some(Self::ext_params_clear),
                request_flush: Some(Self::ext_params_request_flush),
            },
            clap_host_state: clap_host_state {
                mark_dirty: Some(Self::ext_state_mark_dirty),
            },
            clap_host_thread_check: clap_host_thread_check {
                is_main_thread: Some(Self::ext_thread_check_is_main_thread),
                is_audio_thread: Some(Self::ext_thread_check_is_audio_thread),
            },
        })
    }

    /// Register a plugin instance with the host. This is used to keep track of things like audio
    /// thread IDs and pending callbacks. This also contains the `*const clap_host` that should be
    /// paased to the plugin when its created.
    ///
    /// The plugin should be unregistered using
    /// [`unregister_instance()`][Self::unregister_instance()] when it gets destroyed.
    ///
    /// # Panics
    ///
    /// Panics if `instance.plugin` is `None`, or if the instance has already been registered.
    pub fn register_instance(&self, instance: Pin<Arc<HostPluginInstance>>) {
        let previous_instance = self.instances.write().insert(
            instance.plugin.load().expect(
                "'HostPluginInstance::plugin' should contain the plugin's handle when registering \
                 it with the host",
            ),
            instance.clone(),
        );
        assert!(
            previous_instance.is_none(),
            "The plugin instance has already been registered"
        );
    }

    /// Remove a plugin from the list of registered plugins.
    pub fn unregister_instance(&self, instance: Pin<Arc<HostPluginInstance>>) {
        let removed_instance = self
            .instances
            .write()
            .remove(&instance.plugin.load().expect(
                "'HostPluginInstance::plugin' should contain the plugin's handle when \
                 unregistering it with the host",
            ));
        assert!(
            removed_instance.is_some(),
            "Tried unregistering a plugin instance that has not been registered with the host"
        )
    }

    /// Check if any of the host's callbacks were called from the wrong thread. Returns the first
    /// error if this happened.
    pub fn thread_safety_check(&self) -> Result<()> {
        match self.thread_safety_error.lock().take() {
            Some(err) => anyhow::bail!(err),
            None => Ok(()),
        }
    }

    /// Checks whether this is the main thread. If it is not, then an error indicating this can be
    /// retrieved using [`thread_safety_check()`][Self::thread_safety_check()]. Subsequent thread
    /// safety errors will not overwrite earlier ones.
    pub fn assert_main_thread(&self, function_name: &str) {
        let mut thread_safety_error = self.thread_safety_error.lock();
        let current_thread_id = std::thread::current().id();

        match *thread_safety_error {
            // Don't overwrite the first error
            None if std::thread::current().id() != self.main_thread_id => {
                *thread_safety_error = Some(format!(
                    "'{}' may only be called from the main thread (thread {:?}), but it was \
                     called from thread {:?}",
                    function_name, self.main_thread_id, current_thread_id
                ))
            }
            _ => (),
        }
    }

    /// Checks whether this is the audio thread. If it is not, then an error indicating this can be
    /// retrieved using [`thread_safety_check()`][Self::thread_safety_check()]. Subsequent thread
    /// safety errors will not overwrite earlier ones.
    #[allow(unused)]
    pub fn assert_audio_thread(&self, function_name: &str) {
        let current_thread_id = std::thread::current().id();
        if !self.is_audio_thread(current_thread_id) {
            let mut thread_safety_error = self.thread_safety_error.lock();

            match *thread_safety_error {
                None if current_thread_id == self.main_thread_id => {
                    *thread_safety_error = Some(format!(
                        "'{}' may only be called from an audio thread, but it was called from the \
                         main thread",
                        function_name,
                    ))
                }
                None => {
                    *thread_safety_error = Some(format!(
                        "'{}' may only be called from an audio thread, but it was called from an \
                         unknown thread",
                        function_name,
                    ))
                }
                _ => (),
            }
        }
    }

    /// Checks whether this is **not** the audio thread. If it is, then an error indicating this can
    /// be retrieved using [`thread_safety_check()`][Self::thread_safety_check()]. Subsequent thread
    /// safety errors will not overwrite earlier ones.
    #[allow(unused)]
    pub fn assert_not_audio_thread(&self, function_name: &str) {
        let current_thread_id = std::thread::current().id();
        if self.is_audio_thread(current_thread_id) {
            let mut thread_safety_error = self.thread_safety_error.lock();
            if thread_safety_error.is_none() {
                *thread_safety_error = Some(format!(
                    "'{}' was called from an audio thread, this is not allowed",
                    function_name,
                ))
            }
        }
    }

    unsafe extern "C" fn get_extension(
        host: *const clap_host,
        extension_id: *const c_char,
    ) -> *const c_void {
        check_null_ptr!(std::ptr::null(), host, extension_id);
        let (_, this) = HostPluginInstance::from_clap_host_ptr(host);

        // Right now there's no way to have the host only expose certain extensions. We can always
        // add that when test cases need it.
        let extension_id_cstr = CStr::from_ptr(extension_id);
        if extension_id_cstr == CLAP_EXT_AUDIO_PORTS {
            &this.clap_host_audio_ports as *const _ as *const c_void
        } else if extension_id_cstr == CLAP_EXT_NOTE_PORTS {
            &this.clap_host_note_ports as *const _ as *const c_void
        } else if extension_id_cstr == CLAP_EXT_PARAMS {
            &this.clap_host_params as *const _ as *const c_void
        } else if extension_id_cstr == CLAP_EXT_STATE {
            &this.clap_host_state as *const _ as *const c_void
        } else if extension_id_cstr == CLAP_EXT_THREAD_CHECK {
            &this.clap_host_thread_check as *const _ as *const c_void
        } else {
            std::ptr::null()
        }
    }

    /// Returns whether the thread ID is one of the registered audio threads.
    fn is_audio_thread(&self, thread_id: ThreadId) -> bool {
        self.instances
            .read()
            .values()
            .any(|instance| instance.audio_thread.load() == Some(thread_id))
    }

    unsafe extern "C" fn request_restart(host: *const clap_host) {
        check_null_ptr!((), host);

        log::trace!("TODO: Add callbacks for 'clap_host::request_restart()'");
    }

    unsafe extern "C" fn request_process(host: *const clap_host) {
        check_null_ptr!((), host);

        log::trace!("TODO: Add callbacks for 'clap_host::request_process()'");
    }

    unsafe extern "C" fn request_callback(host: *const clap_host) {
        check_null_ptr!((), host);

        log::trace!("TODO: Add callbacks for 'clap_host::request_callback()'");
    }

    unsafe extern "C" fn ext_audio_ports_is_rescan_flag_supported(
        host: *const clap_host,
        _flag: u32,
    ) -> bool {
        check_null_ptr!(false, host);
        let (_, this) = HostPluginInstance::from_clap_host_ptr(host);

        this.assert_main_thread("clap_host_audio_ports::is_rescan_flag_supported()");
        log::trace!("TODO: Add callbacks for 'clap_host_audio_ports::is_rescan_flag_supported()'");

        true
    }

    unsafe extern "C" fn ext_audio_ports_rescan(host: *const clap_host, _flags: u32) {
        check_null_ptr!((), host);
        let (_, this) = HostPluginInstance::from_clap_host_ptr(host);

        this.assert_main_thread("clap_host_audio_ports::rescan()");
        log::trace!("TODO: Add callbacks for 'clap_host_audio_ports::rescan()'");
    }

    unsafe extern "C" fn ext_note_ports_supported_dialects(
        host: *const clap_host,
    ) -> clap_note_dialect {
        check_null_ptr!(0, host);
        let (_, this) = HostPluginInstance::from_clap_host_ptr(host);

        this.assert_main_thread("clap_host_note_ports::supported_dialects()");

        CLAP_NOTE_DIALECT_CLAP | CLAP_NOTE_DIALECT_MIDI | CLAP_NOTE_DIALECT_MIDI_MPE
    }

    unsafe extern "C" fn ext_note_ports_rescan(host: *const clap_host, _flags: u32) {
        check_null_ptr!((), host);
        let (_, this) = HostPluginInstance::from_clap_host_ptr(host);

        this.assert_main_thread("clap_host_note_ports::rescan()");
        log::trace!("TODO: Add callbacks for 'clap_host_note_ports::rescan()'");
    }

    unsafe extern "C" fn ext_params_rescan(
        host: *const clap_host,
        _flags: clap_param_rescan_flags,
    ) {
        check_null_ptr!((), host);
        let (_, this) = HostPluginInstance::from_clap_host_ptr(host);

        this.assert_main_thread("clap_host_params::rescan()");
        log::trace!("TODO: Add callbacks for 'clap_host_params::rescan()'");
    }

    unsafe extern "C" fn ext_params_clear(
        host: *const clap_host,
        _param_id: clap_id,
        _flags: clap_param_clear_flags,
    ) {
        check_null_ptr!((), host);
        let (_, this) = HostPluginInstance::from_clap_host_ptr(host);

        this.assert_main_thread("clap_host_params::clear()");
        log::trace!("TODO: Add callbacks for 'clap_host_params::clear()'");
    }

    unsafe extern "C" fn ext_params_request_flush(host: *const clap_host) {
        check_null_ptr!((), host);
        let (_, this) = HostPluginInstance::from_clap_host_ptr(host);

        this.assert_not_audio_thread("clap_host_params::request_flush()");
        log::trace!("TODO: Add callbacks for 'clap_host_params::request_flush()'");
    }

    unsafe extern "C" fn ext_state_mark_dirty(host: *const clap_host) {
        check_null_ptr!((), host);
        let (_, this) = HostPluginInstance::from_clap_host_ptr(host);

        this.assert_main_thread("clap_host_state::mark_dirty()");
        log::trace!("TODO: Add callbacks for 'clap_host_state::mark_dirty()'");
    }

    unsafe extern "C" fn ext_thread_check_is_main_thread(host: *const clap_host) -> bool {
        check_null_ptr!(false, host);
        let (_, this) = HostPluginInstance::from_clap_host_ptr(host);

        std::thread::current().id() == this.main_thread_id
    }

    unsafe extern "C" fn ext_thread_check_is_audio_thread(host: *const clap_host) -> bool {
        check_null_ptr!(false, host);
        let (_, this) = HostPluginInstance::from_clap_host_ptr(host);

        this.is_audio_thread(std::thread::current().id())
    }
}