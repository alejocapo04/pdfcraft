//! Optional file-provider adapter. The PDF engine remains responsible for PDF serialization,
//! permissions, encryption and incremental updates; the browser host only transports bytes.

use crate::file_providers::{Capture, Command, Host};
use crate::{PdfCraftApp, SaveTarget};
use pdfcraft_engine::DocId;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

type After = Box<dyn FnOnce(&mut PdfCraftApp) + Send>;
struct Saved {
    bytes: Arc<Vec<u8>>,
    after: Option<After>,
}
struct Completed {
    doc: DocId,
    generation: u64,
    after: After,
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
    fn check_password_prompt(&mut self, app: &PdfCraftApp) {
        let Some((request, _, expected)) = &self.opening else { return };
        if app.password_prompt.as_ref().is_some_and(|prompt| Arc::ptr_eq(expected, &prompt.bytes)) {
            return;
        }
        self.host.opened(request, None, "The password prompt was superseded");
        self.opening = None;
    }
    fn publish(&mut self, app: &PdfCraftApp) {
        self.after.retain(|doc, _| self.host.pending_for(*doc));
        let docs = app.session.docs();
        let ids = docs.iter().map(|d| d.id.0).collect::<Vec<_>>();
        let snapshot = docs
            .iter()
            .map(|doc| {
                let drafts = app.views.iter().position(|view| view.id == doc.id).is_some_and(|i| app.has_uncommitted_work(i));
                json!({"documentId":doc.id.0.to_string(),"revision":doc.edit_generation(),"name":doc.name,
                "dirty":doc.dirty||drafts,"providerBound":self.host.bound(doc.id.0),"pending":self.host.pending_for(doc.id.0)})
            })
            .collect::<Vec<_>>();
        self.host.publish_documents(json!(snapshot), &ids);
        if let Some(publish) = app.file_provider_publish {
            publish(&self.host);
        }
    }
}

fn device_download(name: &str, bytes: &[u8]) -> Result<(), String> {
    #[cfg(target_arch = "wasm32")]
    {
        crate::editing::download(name, bytes)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (name, bytes);
        Err("Device downloads are provided by the browser host".into())
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

    fn provider_capture(
        &mut self,
        bridge: &mut Bridge,
        request: &str,
        doc: u64,
        destination: &str,
        name: String,
        save_as: bool,
    ) -> Result<(), String> {
        let id = DocId(doc);
        let after = bridge.after.remove(&doc);
        if !name.to_ascii_lowercase().ends_with(".pdf") {
            return Err("PDF saves require a .pdf filename".into());
        }
        if !self.commit_typing_in(id) || !self.flatten_fill_sign_for_save(id) {
            return Err("The document's pending edits could not be committed".into());
        }
        let current = self.session.get(id).ok_or("The document was closed")?;
        let revision = current.edit_generation();
        // Device copies use the official web serializer; only a provider Save As asks for a rewrite.
        let full = destination == "provider" && save_as && current.read_only_reason.is_none() && !current.is_signed();
        let bytes = if full { self.session.save_full_bytes(id) } else { self.session.save_bytes(id) }.map_err(|e| e.to_string())?;
        self.session.check_save(id, &bytes).map_err(|e| e.to_string())?;
        if destination == "device" {
            device_download(&name, &bytes)?;
            bridge.host.finish_device(request);
            return Ok(());
        }
        let retained = bridge.captured.values().fold(0usize, |n, item| n.saturating_add(item.bytes.len()));
        if retained.saturating_add(bytes.len()) > 80 * 1024 * 1024 {
            return Err("Pending provider snapshots exceed 80 MiB".into());
        }
        bridge.host.capture(request, Capture { doc, revision, name, save_as }, bytes.as_ref().clone())?;
        bridge.captured.insert(doc, Saved { bytes, after });
        Ok(())
    }

    fn provider_result(&mut self, bridge: &mut Bridge, capture: Capture, resource: String, status: &str) -> Option<Completed> {
        let saved = bridge.captured.remove(&capture.doc)?;
        if status != "saved" {
            return None;
        }
        for alias in bridge.host.bind(&capture, resource) {
            let _ = self.session.invalidate_saved(DocId(alias));
        }
        let id = DocId(capture.doc);
        let index = self.views.iter().position(|view| view.id == id)?;
        // The model generation alone misses text still held in a form/content draft.
        // Reopening the captured PDF or closing its tab must not discard that newer UI work.
        if self.has_uncommitted_work(index) || !self.session.get(id).is_some_and(|d| d.edit_generation() == capture.revision) {
            return None;
        }
        if let Err(error) = self.session.mark_saved_external(id, saved.bytes, capture.name) {
            self.notify_fmt("Saved, but reopening failed: {e}", &[("e", &error.to_string())]);
            return None;
        }
        let doc = self.session.get(id)?;
        if let Some(view) = self.views.get_mut(index) {
            view.document_changed(&doc.info);
        }
        saved.after.map(|after| Completed { doc: id, generation: doc.edit_generation(), after })
    }

    fn provider_begin_open(&mut self, mut bridge: Bridge, request: String, name: String, bytes: Vec<u8>) -> Option<Bridge> {
        if !name.to_ascii_lowercase().ends_with(".pdf") || bridge.opening.is_some() || self.password_prompt.is_some() {
            bridge.host.opened(&request, None, "Finish the pending password prompt, and select a PDF file");
            return Some(bridge);
        }
        let bytes = Arc::new(bytes);
        bridge.opening = Some((request, name.clone(), bytes.clone()));
        self.file_provider = Some(bridge);
        self.open_provider_pdf(&name, bytes);
        self.file_provider.take()
    }

    fn provider_finish_after(&mut self, completed: Completed) {
        let Some(index) = self.views.iter().position(|view| view.id == completed.doc) else { return };
        let exact = self.session.get(completed.doc).is_some_and(|doc| doc.edit_generation() == completed.generation && !doc.dirty);
        let pending = self.file_provider.as_ref().is_some_and(|bridge| bridge.host.pending_for(completed.doc.0));
        // Later results can overwrite this resource, and earlier callbacks can edit another tab.
        // Check again at execution, rather than trusting the state when the callback was queued.
        // Logic runs before widgets consume this frame's input. Do not close a tab out
        // from under input that has not reached its draft/model yet. A skipped close is safe.
        let input = self.ctx.as_ref().is_some_and(|ctx| ctx.input(|input| !input.events.is_empty()));
        if exact && !pending && !input && !self.has_uncommitted_work(index) {
            (completed.after)(self);
        }
    }

    /// Called on the logic frame. JS callbacks never borrow an editor or execute commands.
    pub(crate) fn file_provider_tick(&mut self) {
        let Some(mut bridge) = self.file_provider.take() else { return };
        bridge.check_password_prompt(self);
        let mut completed = Vec::new();
        for command in bridge.host.take_commands() {
            match command {
                Command::Save { request, doc, destination, name, save_as } => {
                    if let Err(error) = self.provider_capture(&mut bridge, &request, doc, &destination, name, save_as) {
                        bridge.host.failed(&request, &error);
                        self.notify_fmt("Couldn't save: {e}", &[("e", &error)]);
                    }
                }
                Command::Result { capture, resource, status } => {
                    if let Some(after) = self.provider_result(&mut bridge, capture, resource, &status) {
                        completed.push(after);
                    }
                }
                Command::Open { request, name, bytes, .. } => {
                    let Some(restored) = self.provider_begin_open(bridge, request, name, bytes) else { return };
                    bridge = restored;
                }
            }
        }
        bridge.publish(self);
        self.file_provider = Some(bridge);
        for after in completed {
            self.provider_finish_after(after);
        }
    }

    pub(crate) fn is_provider_open_buffer(&self, bytes: &Arc<Vec<u8>>) -> bool {
        self.file_provider.as_ref().and_then(|bridge| bridge.opening.as_ref()).is_some_and(|(_, _, expected)| Arc::ptr_eq(expected, bytes))
    }

    /// A different local open may replace the password dialog; release only the matching provider read.
    pub(crate) fn provider_supersede_password(&mut self, incoming: &Arc<Vec<u8>>) {
        let abandoned = self
            .password_prompt
            .as_ref()
            .filter(|prompt| !Arc::ptr_eq(&prompt.bytes, incoming) && self.is_provider_open_buffer(&prompt.bytes))
            .map(|prompt| prompt.bytes.clone());
        if let Some(bytes) = abandoned {
            self.password_prompt = None;
            self.provider_opened_pdf(&bytes, Err("The password prompt was superseded by another open".into()));
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

    fn capture_after(app: &mut PdfCraftApp, host: &Host, after: impl FnOnce(&mut PdfCraftApp) + Send + 'static) -> String {
        let index = app.active.unwrap();
        assert!(!app.save_then(index, SaveTarget::InPlace, after));
        let events = host.take_events();
        let request = events.iter().find(|e| e.metadata["type"] == "saveDialog").unwrap().metadata["requestId"].as_str().unwrap().to_string();
        host.resolve_dialog(&request, "provider", "external.pdf").unwrap();
        app.file_provider_tick();
        let saved = host.take_events().into_iter().find(|e| e.metadata["type"] == "save").unwrap();
        saved.metadata["requestId"].as_str().unwrap().to_string()
    }

    #[test]
    fn acknowledgement_preserves_new_ui_drafts_and_does_not_close_their_tab() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let (mut app, host) = setup();
        assert!(app.apply_edit(Edit::SetFieldValue { name: "name".into(), value: FieldValue::Text("Captured".into()) }));
        let id = app.views[0].id;
        let closed = Arc::new(AtomicBool::new(false));
        let called = closed.clone();
        let request = capture_after(&mut app, &host, move |app| {
            called.store(true, Ordering::SeqCst);
            app.close_tab(0);
        });
        // Text waiting for the next UI frame does not advance the model's generation yet.
        app.views[0].pending_edit = Some(Edit::SetFieldValue { name: "name".into(), value: FieldValue::Text("Typed after capture".into()) });
        host.result(request, "saved".into(), "file-1".into()).unwrap();
        app.file_provider_tick();
        assert!(!closed.load(Ordering::SeqCst));
        assert_eq!(app.views.len(), 1);
        assert!(app.views[0].pending_edit.is_some());
        assert!(app.session.get(id).unwrap().dirty);
        assert_eq!(host.snapshot()["documents"][0]["dirty"], true);
    }

    #[test]
    fn invalidated_alias_rejects_the_capture_from_before_its_overwrite() {
        let (mut app, host) = setup();
        assert!(app.apply_edit(Edit::SetFieldValue { name: "name".into(), value: FieldValue::Text("Captured".into()) }));
        let id = app.views[0].id;
        let (request, _) = capture(&mut app, &host);
        let generation = app.session.get(id).unwrap().edit_generation();
        app.session.invalidate_saved(id).unwrap();
        assert_ne!(app.session.get(id).unwrap().edit_generation(), generation);
        host.result(request, "saved".into(), "file-1".into()).unwrap();
        app.file_provider_tick();
        assert!(app.session.get(id).unwrap().dirty);
    }

    #[test]
    fn close_callbacks_recheck_state_after_later_resource_overwrites_in_the_same_tick() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let (mut app, host) = setup();
        let first = app.views[0].id;
        assert!(app.apply_edit(Edit::SetFieldValue { name: "name".into(), value: FieldValue::Text("First".into()) }));
        let called = Arc::new(AtomicBool::new(false));
        let after = called.clone();
        let first_request = capture_after(&mut app, &host, move |app| {
            after.store(true, Ordering::SeqCst);
            app.close_tab(0);
        });
        app.open_bytes("second.pdf", None, include_bytes!("../tests/data/form.pdf").to_vec()).unwrap();
        assert!(app.apply_edit(Edit::SetFieldValue { name: "name".into(), value: FieldValue::Text("Second".into()) }));
        let (second_request, _) = capture(&mut app, &host);
        host.result(first_request, "saved".into(), "shared-resource".into()).unwrap();
        host.result(second_request, "saved".into(), "shared-resource".into()).unwrap();
        app.file_provider_tick();
        assert_eq!(app.views.len(), 2);
        assert!(!called.load(Ordering::SeqCst));
        assert!(app.session.get(first).unwrap().dirty, "the second overwrite invalidates the first tab");
    }

    fn encrypted_pdf() -> Vec<u8> {
        let mut doc = pdfcraft_cos::Document::open(std::sync::Arc::new(include_bytes!("../tests/data/form.pdf").to_vec())).unwrap();
        doc.set_encryption(&pdfcraft_cos::NewEncryption {
            algorithm: pdfcraft_cos::Algorithm::Aes256,
            user_password: "fixture-user",
            owner_password: "fixture-owner",
            permissions: -4,
            encrypt_metadata: true,
            seed: [7; 32],
        })
        .unwrap();
        pdfcraft_cos::write_full(&doc, &Default::default()).unwrap()
    }
    fn begin_encrypted(app: &mut PdfCraftApp, host: &Host) -> String {
        let connection = host.connect("storage".into(), "Storage".into(), "account-1".into()).unwrap();
        host.set_resources(
            &connection,
            vec![crate::file_providers::Resource { id: "encrypted".into(), name: "encrypted.pdf".into(), updated_at: String::new() }],
        )
        .unwrap();
        let request = host.request_open("encrypted").unwrap();
        host.open(request.clone(), "encrypted.pdf".into(), encrypted_pdf()).unwrap();
        app.file_provider_tick();
        assert!(app.password_prompt.is_some());
        assert!(host.busy());
        request
    }
    #[test]
    fn encrypted_read_keeps_its_correlation_through_wrong_and_correct_passwords() {
        let (mut app, host) = setup();
        let request = begin_encrypted(&mut app, &host);
        // A context exists in the actual app. Provider retries must still use the correlated read path.
        app.ctx = Some(egui::Context::default());
        app.submit_password(Some("wrong-fixture-password".into()));
        assert!(app.password_prompt.is_some());
        assert!(host.busy());
        app.submit_password(Some("fixture-user".into()));
        assert!(app.password_prompt.is_none());
        assert_eq!(app.views.len(), 2);
        assert!(!host.busy());
        assert!(host.take_events().iter().any(|event| event.metadata["type"] == "opened" && event.metadata["requestId"] == request));
    }
    #[test]
    fn opening_a_local_pdf_cancels_only_the_superseded_provider_password_read() {
        let (mut app, host) = setup();
        let request = begin_encrypted(&mut app, &host);
        app.open_bytes("local.pdf", None, include_bytes!("../tests/data/form.pdf").to_vec()).unwrap();
        assert!(app.password_prompt.is_none());
        assert!(!host.busy());
        assert!(host.take_events().iter().any(|event| event.metadata["type"] == "openFailed" && event.metadata["requestId"] == request));
        host.disconnect().unwrap();
    }
    #[test]
    fn a_clean_tab_with_a_pending_save_cannot_be_closed_until_the_choice_finishes() {
        let (mut app, host) = setup();
        let id = app.views[0].id;
        assert!(!app.session.get(id).unwrap().dirty);
        assert!(!app.save_active(SaveTarget::InPlace));
        let events = host.take_events();
        let request = events.iter().find(|e| e.metadata["type"] == "saveDialog").unwrap().metadata["requestId"].as_str().unwrap();
        app.request_close_tab(0);
        assert!(app.close_request.is_some());
        app.close_tab(0);
        assert_eq!(app.views.len(), 1);
        host.resolve_dialog(request, "cancel", "").unwrap();
        app.file_provider_tick();
        app.resolve_close(&egui::Context::default(), None);
        app.close_tab(0);
        assert!(app.views.is_empty());
    }

    #[test]
    fn input_waiting_for_widgets_never_triggers_a_deferred_close() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let (mut app, host) = setup();
        assert!(app.apply_edit(Edit::SetFieldValue { name: "name".into(), value: FieldValue::Text("Captured".into()) }));
        let called = Arc::new(AtomicBool::new(false));
        let after = called.clone();
        let request = capture_after(&mut app, &host, move |app| {
            after.store(true, Ordering::SeqCst);
            app.close_tab(0);
        });
        let ctx = egui::Context::default();
        app.ctx = Some(ctx.clone());
        host.result(request, "saved".into(), "file-1".into()).unwrap();
        let mut output = ctx.run_ui(egui::RawInput { events: vec![egui::Event::Text("Input after capture".into())], ..Default::default() }, |_| {
            app.file_provider_tick()
        });
        output.textures_delta.clear();
        assert!(!called.load(Ordering::SeqCst));
        assert_eq!(app.views.len(), 1);
    }
}
