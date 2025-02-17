from .. import Smoketest

class WasmBindgen(Smoketest):
    AUTOPUBLISH = False
    MODULE_CODE = """
use spacetimedb::{log, spacetimedb};

#[spacetimedb(reducer)]
pub fn test() {
    log::info!("Hello! {}", now());
}

#[wasm_bindgen::prelude::wasm_bindgen]
extern "C" {
    fn now() -> i32;
}
"""
    EXTRA_DEPS = 'wasm-bindgen = "0.2"'

    def test_detect_wasm_bindgen(self):
        """Ensure that spacetime build properly catches wasm_bindgen imports"""

        output = self.spacetime("build", "--project-path", self.project_path, full_output=True, check=False)
        self.assertTrue(output.returncode)
        self.assertIn("wasm-bindgen detected", output.stderr)
