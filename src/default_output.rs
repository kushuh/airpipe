//! Hands the default output back when airpipe quits.
//!
//! Desktop sound settings make an output "the default" by writing
//! `default.configured.audio.sink` in PipeWire's `default` metadata. If the
//! user chose "AirPlay: …" and airpipe then quits, sound should go straight
//! back to the output they used before, not to whatever the session manager
//! picks. So we watch that key from the start, remember the last default
//! that was not ours, and restore it on quit if ours is still the default.
//!
//! Runs on the PipeWire thread (see `sink.rs`).

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::Result;
use pipewire as pw;
use pw::metadata::{Metadata, MetadataListener};
use pw::types::ObjectType;
use tracing::{debug, info};

const KEY: &str = "default.configured.audio.sink";

#[derive(Default)]
struct State {
    metadata: Option<Metadata>,
    listener: Option<MetadataListener>,
    /// The current configured default output (node name).
    current: Option<String>,
    /// The last configured default that was not airpipe's output.
    previous: Option<String>,
}

pub struct DefaultOutput {
    own_node: String,
    state: Rc<RefCell<State>>,
    _registry: pw::registry::RegistryRc,
    _listener: pw::registry::Listener,
}

impl DefaultOutput {
    /// Starts watching the default output. `own_node` is our sink's node name.
    pub fn watch(core: &pw::core::CoreRc, own_node: &str) -> Result<Self> {
        let registry = core.get_registry_rc()?;
        let state = Rc::new(RefCell::new(State::default()));
        let listener = registry
            .add_listener_local()
            .global({
                let registry = registry.downgrade();
                let state = state.clone();
                let own_node = own_node.to_string();
                move |global| {
                    let is_default_metadata = global.type_ == ObjectType::Metadata
                        && global.props.as_ref().and_then(|p| p.get("metadata.name")) == Some("default");
                    if !is_default_metadata || state.borrow().metadata.is_some() {
                        return;
                    }
                    let Some(registry) = registry.upgrade() else { return };
                    let Ok(metadata) = registry.bind::<Metadata, _>(global) else { return };
                    let listener = metadata
                        .add_listener_local()
                        .property({
                            let state = state.clone();
                            let own_node = own_node.clone();
                            move |_subject, key, _type, value| {
                                if key == Some(KEY) {
                                    let name = value.and_then(parse_name);
                                    let mut s = state.borrow_mut();
                                    if let Some(n) = &name
                                        && n != &own_node
                                    {
                                        s.previous = Some(n.clone());
                                    }
                                    s.current = name;
                                }
                                0
                            }
                        })
                        .register();
                    let mut s = state.borrow_mut();
                    s.metadata = Some(metadata);
                    s.listener = Some(listener);
                }
            })
            .register();
        Ok(Self { own_node: own_node.to_string(), state, _registry: registry, _listener: listener })
    }

    /// If our output is the configured default, makes the previous default
    /// the default again (or clears the setting, so the session manager picks
    /// the best output). Returns true if a change was sent to PipeWire.
    pub fn restore(&self) -> bool {
        let s = self.state.borrow();
        let (Some(metadata), Some(current)) = (&s.metadata, &s.current) else { return false };
        if current != &self.own_node {
            return false;
        }
        match &s.previous {
            Some(previous) => {
                info!("restoring the default output to {previous:?}");
                metadata.set_property(0, KEY, Some("Spa:String:JSON"), Some(&name_json(previous)));
            }
            None => {
                debug!("no previous default output known; letting the session manager choose");
                metadata.set_property(0, KEY, None, None);
            }
        }
        true
    }
}

/// Extracts `name` from the metadata JSON value, e.g. `{ "name": "alsa_output…" }`.
fn parse_name(json: &str) -> Option<String> {
    let after_key = &json[json.find("\"name\"")? + "\"name\"".len()..];
    let after_colon = after_key.trim_start().strip_prefix(':')?.trim_start();
    let value = after_colon.strip_prefix('"')?;
    Some(value[..value.find('"')?].to_string())
}

/// Builds the metadata JSON value for a node name. Node names are plain
/// identifiers; quotes and backslashes are dropped to keep the JSON valid.
fn name_json(name: &str) -> String {
    let safe: String = name.chars().filter(|c| *c != '"' && *c != '\\').collect();
    format!("{{ \"name\": \"{safe}\" }}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_values_round_trip() {
        assert_eq!(
            parse_name(r#"{ "name": "alsa_output.pci-0000_65_00.6.analog-stereo" }"#).as_deref(),
            Some("alsa_output.pci-0000_65_00.6.analog-stereo")
        );
        assert_eq!(parse_name(r#"{"name":"airpipe"}"#).as_deref(), Some("airpipe"));
        assert_eq!(parse_name("garbage"), None);
        assert_eq!(parse_name(&name_json("bad\"name")).as_deref(), Some("badname"));
    }
}
