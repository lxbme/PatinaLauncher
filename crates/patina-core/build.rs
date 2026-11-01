fn main() {
    println!("cargo:rerun-if-env-changed=PATINA_MSA_CLIENT_ID");
    println!("cargo:rerun-if-env-changed=PATINA_CURSEFORGE_API_KEY");
    // rust-i18n does not track the locale files itself.
    println!("cargo:rerun-if-changed=locales");
}
