// Small progressive enhancements on top of htmx. Pages work without this file.

// Bulk edit: "select all" toggles every bookmark checkbox in the same form.
document.addEventListener("change", (e) => {
  if (!e.target.matches("[data-select-all]")) return;
  e.target.form.querySelectorAll('input[name="bookmark_id"]').forEach((c) => (c.checked = e.target.checked));
});

// Details: htmx swaps the <dialog> into #modal; show it as a real modal and drop it when closed.
document.addEventListener("htmx:afterSwap", (e) => {
  const dialog = e.detail.target.id === "modal" && e.detail.target.querySelector("dialog");
  if (!dialog) return;
  dialog.removeAttribute("open");
  dialog.addEventListener("close", () => dialog.remove());
  dialog.showModal();
});
document.addEventListener("click", (e) => {
  const close = e.target.closest("[data-close]");
  const dialog = close && close.closest("#modal dialog");
  if (!dialog) return; // standalone details page: the link navigates back
  e.preventDefault();
  dialog.close();
});

// Add form: fill empty title/description with the page's metadata from /bookmarks/new/check.
document.addEventListener("htmx:load", (e) => {
  const info = e.detail.elt;
  if (info.id !== "url-check") return;
  for (const field of ["title", "description"]) {
    const input = info.closest("form").querySelector(`[name=${field}]`);
    if (input && !input.value && info.dataset[field]) input.value = info.dataset[field];
  }
});

// Add form: while the page metadata loads, mark the empty title/description fields as busy.
// Requests for an empty URL (e.g. the form opened without ?url=) are skipped.
function metadataFields(e) {
  const input = e.detail.elt;
  return input.name === "url" && input.hasAttribute("hx-indicator")
    ? ["title", "description"].map((f) => input.form.querySelector(`[name=${f}]`))
    : [];
}
document.addEventListener("htmx:beforeRequest", (e) => {
  const fields = metadataFields(e);
  if (!fields.length) return;
  if (!e.detail.elt.value.trim()) return e.preventDefault();
  for (const f of fields.filter((f) => !f.value)) {
    f.setAttribute("aria-busy", "true");
    f.placeholder = "Fetching from the page…";
  }
});
document.addEventListener("htmx:afterRequest", (e) => {
  for (const f of metadataFields(e)) {
    f.removeAttribute("aria-busy");
    f.placeholder = "";
  }
});
