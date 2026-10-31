//! Optional file-provider adapter. The PDF engine remains responsible for PDF serialization,
//! permissions, encryption and incremental updates; the browser host only transports bytes.

use std::collections::HashMap;
use std::sync::Arc;

use pdfcraft_engine::DocId;
use serde_json::json;

use crate::file_providers::{Capture, Command, Host};
use crate::{PdfCraftApp, SaveTarget};

type After = Box<dyn FnOnce(&mut PdfCraftApp) + Send>;

struct Saved {
    bytes: Arc<Vec<u8>>,
    after: Option<After>,
}

/// Installed only by an explicitly participating web host. Constructing it does not register
/// or enable a provider. Account identities and paths never become PDF document properties.
pub struct Bridge {
    pub host: Host,
    after: HashMap<u64, After>,
    captured: HashMap<u64, Saved>,
    opening: Option<(String, String, Arc<Vec<u8>>)>,
}

impl Bridge {
    pub fn new(host: Host) -> Self {
        Self { host, after: HashMap::new(), captured: HashMap::new(), opening: None }
    }
}

impl PdfCraftApp {
    pub(crate) fn provider_choose(&mut self, index: usize, target: SaveTarget, after: After) -> bool {
        let Some(view) = self.views.get(index) else { return false };
        let Some(doc) = self.session.get(view.id) else { return false };
        let Some(bridge) = self.file_provider.as_mut() else { return false };
        if bridge.host.pending_for(doc.id.0) {
            return false;
        }
        let name = bridge
            .host
            .filename(doc.id.0)
            .unwrap_or_else(|| if doc.name.to_ascii_lowercase().ends_with(".pdf") { doc.name.clone() } else { format!("{}.pdf", doc.name) });
        match bridge.host.choose(doc.id.0, doc.edit_generation(), name, target == SaveTarget::As) {
            Ok(()) => {
                bridge.after.insert(doc.id.0, after);
            }
            Err(error) => self.notify_fmt("Couldn't save: {e}", &[("e", &error)]),
        }
        if let Some(ctx) = &self.ctx {
            ctx.request_repaint();
        }
        false
    }

    /// Called on the logic frame. JS callbacks never borrow an editor or execute commands.
    pub(crate) fn file_provider_tick(&mut self) {
        let Some(mut bridge) = self.file_provider.take() else { return };
        let mut completed: Vec<After> = Vec::new();
        for command in bridge.host.take_commands() {
            match command {
                Command::Save { request, doc, destination, name, save_as } => {
                    let id = DocId(doc);
                    let after = bridge.after.remove(&doc);
                    let result = (|| {
                        if !name.to_ascii_lowercase().ends_with(".pdf") {
                            return Err("PDF saves require a .pdf filename".to_string());
                        }
                        if !self.commit_typing_in(id) || !self.flatten_fill_sign_for_save(id) {
                            return Err("The document's pending edits could not be committed".into());
                        }
                        let current = self.session.get(id).ok_or("The document was closed")?;
                        let revision = current.edit_generation();
                        let full = save_as && current.read_only_reason.is_none() && !current.is_signed();
                        let bytes = if full { self.session.save_full_bytes(id) } else { self.session.save_bytes(id) }.map_err(|e| e.to_string())?;
                        self.session.check_save(id, &bytes).map_err(|e| e.to_string())?;
                        if destination == "device" {
                            #[cfg(target_arch = "wasm32")]
                            crate::editing::download(&name, &bytes)?;
                            #[cfg(not(target_arch = "wasm32"))]
                            return Err("Device downloads are provided by the browser host".into());
                            #[cfg(target_arch = "wasm32")]
                            {
                                bridge.host.finish_device(&request);
                                return Ok(());
                            }
                        }
                        let retained = bridge.captured.values().fold(0usize, |n, item| n.saturating_add(item.bytes.len()));
                        if retained.saturating_add(bytes.len()) > 80 * 1024 * 1024 {
                            return Err("Pending provider snapshots exceed 80 MiB".into());
                        }
                        bridge.host.capture(&request, Capture { doc, revision, name, save_as }, bytes.as_ref().clone())?;
                        bridge.captured.insert(doc, Saved { bytes, after });
                        Ok(())
                    })();
                    if let Err(error) = result {
                        bridge.host.failed(&request, &error);
                        self.notify_fmt("Couldn't save: {e}", &[("e", &error)]);
                    }
                }
                Command::Result { capture, resource, status } => {
                    let Some(saved) = bridge.captured.remove(&capture.doc) else { continue };
                    if status != "saved" {
                        continue;
                    }
                    let aliases = bridge.host.bind(&capture, resource);
                    for alias in aliases {
                        let _ = self.session.invalidate_saved(DocId(alias));
                    }
                    let id = DocId(capture.doc);
                    // mark_saved reparses and rebases the PDF. Never run it on newer edits.
                    if self.session.get(id).is_some_and(|d| d.edit_generation() == capture.revision) {
                        match self.session.mark_saved_external(id, saved.bytes, capture.name) {
                            Ok(()) => {
                                if let Some(doc) = self.session.get(id)
                                    && let Some(view) = self.views.iter_mut().find(|v| v.id == id)
                                {
                                    view.document_changed(&doc.info);
                                }
                                if let Some(after) = saved.after {
                                    completed.push(after);
                                }
                            }
                            Err(error) => self.notify_fmt("Saved, but reopening failed: {e}", &[("e", &error.to_string())]),
                        }
                    }
                }
                Command::Open { request, name, bytes, .. } => {
                    if !name.to_ascii_lowercase().ends_with(".pdf") || bridge.opening.is_some() || self.password_prompt.is_some() {
                        bridge.host.opened(&request, None, "Finish the pending password prompt, and select a PDF file");
                        continue;
                    }
                    let bytes = Arc::new(bytes);
                    bridge.opening = Some((request, name.clone(), bytes.clone()));
                    // Return the bridge while the native open path correlates success/passwords.
                    self.file_provider = Some(bridge);
                    self.open_provider_pdf(&name, bytes);
                    let Some(restored) = self.file_provider.take() else { return };
                    bridge = restored;
                }
            }
        }
        bridge.after.retain(|doc, _| bridge.host.pending_for(*doc));
        let docs = self.session.docs();
        let ids = docs.iter().map(|d| d.id.0).collect::<Vec<_>>();
        bridge.host.publish_documents(
            json!(
                docs.iter()
                    .map(|d| json!({
                        "documentId":d.id.0.to_string(), "revision":d.edit_generation(), "name":d.name,
                        "dirty":d.dirty, "providerBound":bridge.host.bound(d.id.0), "pending":bridge.host.pending_for(d.id.0)
                    }))
                    .collect::<Vec<_>>()
            ),
            &ids,
        );
        if let Some(publish) = self.file_provider_publish {
            publish(&bridge.host);
        }
        self.file_provider = Some(bridge);
        for after in completed {
            after(self);
        }
    }

    pub(crate) fn provider_opened_pdf(&mut self, bytes: &Arc<Vec<u8>>, result: Result<DocId, String>) {
        let Some(bridge) = &mut self.file_provider else { return };
        if !bridge.opening.as_ref().is_some_and(|(_, _, expected)| Arc::ptr_eq(expected, bytes)) {
            return;
        }
        let Some((request, name, _)) = bridge.opening.take() else { return };
        match result {
            Ok(id) => {
                let capture = self.session.get(id).map(|d| Capture { doc: id.0, revision: d.edit_generation(), name, save_as: false });
                bridge.host.opened(&request, capture, "The document could not be admitted");
            }
            Err(error) => bridge.host.opened(&request, None, &error),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::Bridge;
    use crate::file_providers::Host;
    use crate::{PdfCraftApp, SaveTarget};
    use pdfcraft_engine::{Edit, FieldValue};

    fn setup() -> (PdfCraftApp, Host) {
        let mut app = PdfCraftApp::new();
        app.open_bytes("source.pdf", None, include_bytes!("../tests/data/form.pdf").to_vec()).unwrap();
        let host = Host::default();
        host.set_session("pdf-test-session-0001".into()).unwrap();
        host.connect("storage".into(), "Storage".into(), "account-1".into()).unwrap();
        app.file_provider = Some(Bridge::new(host.clone()));
        (app, host)
    }

    fn capture(app: &mut PdfCraftApp, host: &Host) -> (String, Vec<u8>) {
        assert!(!app.save_active(SaveTarget::InPlace));
        let events = host.take_events();
        let request = events.iter().find(|e| e.metadata["type"] == "saveDialog").unwrap().metadata["requestId"].as_str().unwrap();
        host.resolve_dialog(request, "provider", "external.pdf").unwrap();
        app.file_provider_tick();
        let event = host.take_events().into_iter().find(|e| e.metadata["type"] == "save").unwrap();
        (event.metadata["requestId"].as_str().unwrap().to_string(), event.bytes.unwrap())
    }

    #[test]
    fn pdf_provider_ack_preserves_newer_edits_and_reopens_native_pdf() {
        let (mut app, host) = setup();
        assert!(app.apply_edit(Edit::SetFieldValue { name: "name".into(), value: FieldValue::Text("Captured".into()) }));
        let id = app.views[0].id;
        let (request, bytes) = capture(&mut app, &host);
        assert!(bytes.starts_with(b"%PDF-"));
        assert!(app.session.get(id).unwrap().dirty, "transport is not a save acknowledgement");
        assert!(app.apply_edit(Edit::SetFieldValue { name: "name".into(), value: FieldValue::Text("Newer".into()) }));
        host.result(request, "saved".into(), "file-1".into()).unwrap();
        app.file_provider_tick();
        assert!(app.session.get(id).unwrap().dirty);
        let read = host.request_open("file-1").unwrap();
        host.open(read, "external.pdf".into(), bytes).unwrap();
        app.file_provider_tick();
        assert_eq!(app.views.len(), 2);
        assert_ne!(app.views[1].id, id);
        assert!(!app.session.get(app.views[1].id).unwrap().dirty);
        assert!(host.take_events().iter().any(|e| e.metadata["type"] == "opened"));
    }

    #[test]
    fn pdf_provider_only_cleans_acknowledged_generation_and_default_is_off() {
        assert!(PdfCraftApp::new().file_provider.is_none());
        assert!(!Host::default().enabled());
        let (mut app, host) = setup();
        assert!(app.apply_edit(Edit::SetFieldValue { name: "name".into(), value: FieldValue::Text("Saved".into()) }));
        let id = app.views[0].id;
        let (request, _) = capture(&mut app, &host);
        host.result(request, "saved".into(), "file-1".into()).unwrap();
        app.file_provider_tick();
        let doc = app.session.get(id).unwrap();
        assert!(!doc.dirty);
        assert!(doc.path.is_none());
        assert_eq!(doc.name, "external.pdf");
    }
}
