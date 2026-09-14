fn main() {
    let config = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer);
    slint_build::compile_with_config("ui/nfc_writer.slint", config)
        .expect("failed to compile NFC Writer UI");

    println!("cargo:rerun-if-changed=ui/nfc_writer.slint");
}
