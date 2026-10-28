fn main() {
    // rust-i18n does not track the locale files itself.
    println!("cargo:rerun-if-changed=locales");
}
