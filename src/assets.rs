// 前端静态资源：include_str! 内联进二进制。
// 这些文件必须保存为 UTF-8（无 BOM）——中文 Windows 的记事本默认 ANSI 会编译失败。

pub const GLASS_CSS: &str = include_str!("../assets/glass.css");
pub const COUNTER_JS: &str = include_str!("../assets/counter.js");
pub const INDEX_HTML: &str = include_str!("../assets/index.html");
pub const WEB_CSS: &str = include_str!("../assets/web.css");
pub const APP_JS: &str = include_str!("../assets/app.js");

/// 报告样式基座（当前与 Web 共用同一套玻璃拟态）。
pub fn report_style() -> &'static str {
    GLASS_CSS
}
