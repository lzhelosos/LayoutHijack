fn main() {
    // App icon (ID 1) plus both notification-area icons (IDs 101 and 102).
    embed_resource::compile("icons.rc", embed_resource::NONE)
        .manifest_optional()
        .expect("Unable to embed LayoutHijack icons");
}
