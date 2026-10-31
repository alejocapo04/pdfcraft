# Optional web file providers

The bridge is disabled by default. The web app publishes window.pdfcraftFileProviders (protocolVersion 1) and dispatches pdfcraft-file-provider events. Publishing the API does not register a provider. Desktop and CLI hosts do not install it. Existing browser downloads and the legacy ?host=parent integration remain in place until a trusted same-origin host explicitly registers a provider.

## Host contract

registerProvider(id, label, namespace) returns a fresh connectionId. The namespace is an opaque account identifier, never a token or password. Only one identity is active; unregisterProvider rejects while any chooser, serialization, read, password prompt or acknowledgement is pending. setEnabled(false) pauses new provider work without dropping pending results.

setResources(connectionId, [{id, name, updatedAt}]) supplies up to 100 external Recent entries. They appear separately from local Recent files on Home and File > Open Recent. Resource IDs are opaque, not filesystem paths or commands. requestOpen(resourceId) emits openRequest; the host replies using openResource(requestId, filename, Uint8Array) or cancelOpen(requestId, error). Successful admission emits opened with the new session document ID. The native password prompt remains in use for encrypted PDFs, and reads are correlated with the exact input buffer through that prompt.

Save and Save As emit saveDialog with requestId, documentId, revision, name, saveAs and the current resourceId when applicable. The host calls resolveSaveDialog(requestId, destination, filename), where destination is provider, device or cancel. Each explicit Save asks again. Device downloads use the browser writer and do not acknowledge durable provider storage. Other exports retain their existing routes.

The provider destination emits save with destination=provider, immutable bytes and the captured revision. Only after committing those bytes durably may the host call saveResult(requestId, saved, resourceId). Other terminal statuses are failed, cancel and download; none marks the provider document clean. Save As omits the old resource identity. An overwrite invalidates other pathless tabs bound to that resource, without confusing homonymous local files.

PDF saves use the engine's original serializer, reopen validation, encryption and permission checks. Save remains incremental; Save As makes a clean full copy when allowed, while signed and protected documents keep the original safeguards. A saved acknowledgement rebases only the matching document generation. Later edits stay dirty and an old acknowledgement cannot replace them or close that tab. Successful external saves use an external binding and a display name, never a fabricated local path.

## Limits and responsibilities

The trusted host owns authentication, user isolation, authorization, durable writes, conflict/version policy, retry, destination UI and URLs. No backend, service URL, access token, network client, cross-origin messaging or cloud widget is added here. Same-origin script is already trusted to read this editor's data.

The bridge accepts native PDF files only. Each transfer, queued open bytes, emitted byte queue and retained PDF save snapshots is bounded to 80 MiB; there are at most 16 pending operations, 64 queued events and 100 resources. Identity and request IDs use bounded safe characters. JavaScript arrays, strings and typed arrays are checked before copying into WASM. Requests carry a random page nonce and checked monotonic counter; stale connections and duplicate responses are rejected.

A browser download is not evidence that a file reached durable storage. The adapter conservatively leaves its provider checkpoint unchanged for device copies and failed/cancelled operations. Leaving the page while a provider operation is pending retains the native unsaved-work warning.

## Validation

Regression fixtures cover opt-in defaults, account/request isolation, bounded metadata, exact-generation acknowledgement, native PDF serialization/reopen, separate resource identities and delayed acknowledgements after newer edits. They are source fixtures until executed; publication precedes validation for this contribution.
