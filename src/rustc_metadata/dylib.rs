use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::ffi::CString;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::rustc_metadata::rmeta::ProcMacroKind;
use crate::rustc_proc_macro::bridge::client::{Client as ProcMacroClient, CountedProcMacro};
use crate::rustc_span::def_id::StableCrateId;
use eko::path::Path;

#[derive(Clone)]
pub(crate) enum LoadedProcMacro {
    CustomDerive {
        trait_name: String,
        attributes: Vec<String>,
        client: ProcMacroClient,
    },
    Attr {
        name: String,
        client: ProcMacroClient,
    },
    Bang {
        name: String,
        client: ProcMacroClient,
    },
}

impl LoadedProcMacro {
    pub(crate) fn client_for(&self, kind: &ProcMacroKind) -> Option<ProcMacroClient> {
        match (self, kind) {
            (
                Self::CustomDerive {
                    trait_name,
                    attributes,
                    client,
                },
                ProcMacroKind::CustomDerive {
                    trait_name: expected_trait_name,
                    attributes: expected_attributes,
                    ..
                },
            ) if trait_name == expected_trait_name && attributes == expected_attributes => {
                Some(*client)
            }
            (Self::Attr { name, client }, ProcMacroKind::Attr { name: expected, .. })
                if name == expected =>
            {
                Some(*client)
            }
            (Self::Bang { name, client }, ProcMacroKind::Bang { name: expected, .. })
                if name == expected =>
            {
                Some(*client)
            }
            _ => None,
        }
    }
}

#[cfg(unix)]
fn loader_error() -> String {
    use core::ffi::CStr;

    let error = unsafe { libc::dlerror() };
    if error.is_null() {
        "the dynamic loader returned no detail".to_owned()
    } else {
        unsafe { CStr::from_ptr(error) }
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(unix)]
pub(crate) fn dlsym_proc_macros(
    path: &Path,
    stable_crate_id: StableCrateId,
) -> Result<&'static [LoadedProcMacro], String> {
    use core::ffi::CStr;

    let path_c = CString::new(path.as_bytes())
        .map_err(|_| format!("proc-macro dylib path contains a NUL byte: {}", path.display()))?;
    let symbol_name = crate::rustc_session::generate_proc_macro_decls_symbol(
        stable_crate_id,
    );
    let symbol_c = CString::new(symbol_name.as_bytes())
        .map_err(|_| "proc-macro declaration symbol contains a NUL byte".to_owned())?;

    let handle = unsafe { libc::dlopen(path_c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        return Err(format!("could not open {}: {}", path.display(), loader_error()));
    }

    unsafe {
        let _ = libc::dlerror();
    }
    let symbol = unsafe { libc::dlsym(handle, symbol_c.as_ptr()) };
    let error = unsafe { libc::dlerror() };
    if symbol.is_null() || !error.is_null() {
        let reason = if error.is_null() {
            "dlsym returned a null symbol".to_owned()
        } else {
            unsafe { CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned()
        };
        unsafe {
            libc::dlclose(handle);
        }
        return Err(format!(
            "could not load symbol {} from {}: {}",
            symbol_name,
            path.display(),
            reason
        ));
    }

    // The clients contain function pointers into this image, so keep it open for the process lifetime.
    let declarations = unsafe { *symbol.cast::<&'static [CountedProcMacro]>() };
    let loaded = declarations
        .iter()
        .map(|declaration| match *declaration {
            CountedProcMacro::CustomDerive {
                trait_name,
                attributes,
                client,
            } => LoadedProcMacro::CustomDerive {
                trait_name: trait_name.to_owned(),
                attributes: attributes
                    .iter()
                    .map(|attribute| (*attribute).to_owned())
                    .collect(),
                client: client.client(),
            },
            CountedProcMacro::Attr { name, client } => LoadedProcMacro::Attr {
                name: name.to_owned(),
                client: client.client(),
            },
            CountedProcMacro::Bang { name, client } => LoadedProcMacro::Bang {
                name: name.to_owned(),
                client: client.client(),
            },
        })
        .collect::<Vec<_>>();

    Ok(Box::leak(loaded.into_boxed_slice()))
}

#[cfg(not(unix))]
pub(crate) fn dlsym_proc_macros(
    _path: &Path,
    _stable_crate_id: StableCrateId,
) -> Result<&'static [LoadedProcMacro], String> {
    Err("proc-macro dylib loading requires a Unix host".to_owned())
}

