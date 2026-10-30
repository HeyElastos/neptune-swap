fn main() {
    #[cfg(target_arch = "wasm32")]
    console_error_panic_hook::set_once();
    intents_ui::run().expect("Neptune Intents failed to start");
}
