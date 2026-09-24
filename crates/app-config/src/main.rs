//! 設定の確認用 CLI。
//!
//!   app-config show       最終的に有効な設定（出所つき。秘密情報は ***）
//!   app-config get KEY    1 つの項目の値（スクリプト用。例: api.port）
//!   app-config validate   検証だけ行う（この版で未実装の設定が指定されていたら、失敗する）
//!   app-config web-env    web のビルドに渡す環境変数（`eval "$(app-config web-env)"`）
//!
//! 終了コード: 0 = 成功、1 = 設定が不正、2 = 使い方の誤り。

use std::process::ExitCode;

const USAGE: &str = "使い方: app-config show | get KEY | validate | web-env";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let loaded = match app_config::load() {
        Ok(loaded) => loaded,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["show"] => {
            print!("{}", loaded.render());
            ExitCode::SUCCESS
        }
        ["get", key] => match loaded.get(key) {
            Ok(value) => {
                println!("{value}");
                ExitCode::SUCCESS
            }
            Err(reason) => {
                eprintln!("{reason}");
                ExitCode::from(1)
            }
        },
        ["validate"] => match loaded.ensure_supported() {
            Ok(()) => {
                println!("OK: 設定は有効です");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::from(1)
            }
        },
        ["web-env"] => {
            print!("{}", loaded.web_env());
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
