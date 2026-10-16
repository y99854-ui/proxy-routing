// 程序入口。Tauri 在这里注册“命令”：前端 JS 通过 invoke("命令名") 调用这些 Rust 函数。
// 下面这行让 Windows 发布版不弹出黑色命令行窗口（Mac 上无影响）。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod clash;

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            clash::detect_config,
            clash::test_controller,
            clash::check_url,
            clash::quick_check,
            clash::switch_node,
        ])
        .run(tauri::generate_context!())
        .expect("启动 Tauri 应用失败");
}
