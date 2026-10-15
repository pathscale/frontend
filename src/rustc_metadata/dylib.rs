use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;

use crate::rustc_metadata::rmeta::ProcMacroKind;
use crate::rustc_proc_macro::bridge::client::Client as ProcMacroClient;
use crate::rustc_span::def_id::StableCrateId;
use eko::path::Path;

#[derive(Clone)]
#[expect(
    dead_code,
    reason = "preserves proc-macro metadata kinds and clients for host execution support"
)]
pub(crate) enum LoadedProcMacro {
    CustomDerive { trait_name: String, attributes: Vec<String>, client: ProcMacroClient },
    Attr { name: String, client: ProcMacroClient },
    Bang { name: String, client: ProcMacroClient },
}

impl LoadedProcMacro {
    pub(crate) fn client_for(&self, kind: &ProcMacroKind) -> Option<ProcMacroClient> {
        match (self, kind) {
            (
                Self::CustomDerive { trait_name, attributes, client },
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

#[expect(
    dead_code,
    reason = "retains the dynamic proc-macro symbol lookup entrypoint for metadata loading"
)]
pub(crate) fn dlsym_proc_macros(
    _path: &Path,
    _stable_crate_id: StableCrateId,
) -> Result<&'static [LoadedProcMacro], String> {
    Err("native proc-macro execution is disabled".to_owned())
}
