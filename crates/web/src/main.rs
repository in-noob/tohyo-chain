//! wasm のエントリポイント。`trunk` がこれをビルドして `index.html` に組み込む。

use web::App;

fn main() {
    // wasm の panic をブラウザのコンソールに出す。
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
