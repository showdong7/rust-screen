# Rust Screen

以 Rust 製作的 Windows 10 / 11 桌面截圖工具，介面使用 [egui / eframe](https://github.com/emilk/egui)，透過 Win32 GDI 擷取畫面。另附一個 MCP 伺服器，讓 Claude 等 AI 助理也能擷取畫面。

## 功能

- **全域快捷鍵**：預設 `Ctrl + Shift + S`，可自訂（例如 `Alt+F9`、`Ctrl+PrintScreen`）。
- **三種截圖模式**：自訂範圍（直接在螢幕拖曳選取，支援跨螢幕）、全螢幕、作用中視窗。
- **固定尺寸與再裁切**：可指定精確寬高；儲存後也能「調整範圍」再另存。
- **預覽標註**：直線、方形、橢圓、箭頭、繁體中文文字；支援選取、移動、縮放、改色、復原／重做。
- **輸出**：自動命名的 PNG，不覆寫既有檔案，可同時複製到剪貼簿。
- **系統匣常駐**、**單一執行個體**、**System／Dark／Light 主題**，設定會自動保存。

## 編譯與執行

需要 Rust 1.88 以上，以及以下任一工具鏈：

- **MSVC**：Visual Studio「使用 C++ 的桌面開發」與 Windows SDK。
- **GNU**：MinGW-w64（`gcc.exe`、`dlltool.exe`、`windres.exe`）。`run.ps1` 會自動偵測 `C:\msys64\mingw64\bin`。

```powershell
.\run.ps1 -Release              # 編譯並執行
.\run.ps1 -Release -BuildOnly   # 只編譯，輸出 target\release\rust-screen.exe
.\run.ps1 -Test                 # 執行測試
```

工具鏈已在 `PATH` 上時，也可直接使用 `cargo`：

```powershell
cargo run --release --locked
cargo test --locked
```

> 重新編譯前，請先用介面或系統匣選單的「退出程式」結束常駐程序；按 X 只會隱藏到系統匣，執行檔仍會被鎖住。

## 使用方式

1. 啟動程式並選擇截圖模式。
2. 按 `Ctrl + Shift + S`。自訂範圍模式下，直接在螢幕上按住左鍵拖曳，**放開即儲存**；`Esc` 或右鍵取消。
3. 回到程式後可在預覽上標註，按「另存標註 PNG」或「複製圖片」。

| 項目 | 位置 |
| --- | --- |
| 預設圖片資料夾 | `%USERPROFILE%\Pictures\RustScreen` |
| 設定檔 | `%APPDATA%\RustScreen\settings.json`（可用 `RUST_SCREEN_CONFIG_DIR` 指定其他目錄） |

## MCP 伺服器

`rust-screen-mcp.exe` 是 [Model Context Protocol](https://modelcontextprotocol.io/) stdio 伺服器，與主程式共用擷取核心，不需開啟主程式。`run.ps1 -Release -BuildOnly` 會一併編譯。

註冊到 Claude Code：

```powershell
claude mcp add rust-screen -- "C:\path\to\rust-screen-mcp.exe"
```

Claude Desktop（`claude_desktop_config.json`）：

```json
{
  "mcpServers": {
    "rust-screen": { "command": "C:\\path\\to\\rust-screen-mcp.exe" }
  }
}
```

| Tool | 用途 |
| --- | --- |
| `list_monitors` | 列出各螢幕的座標、工作區與 DPI 縮放 |
| `list_windows` | 列出可見的頂層視窗，可依標題篩選 |
| `capture_screen` | 擷取指定螢幕或所有螢幕 |
| `capture_window` | 依 handle 或標題擷取視窗，省略則擷取前景視窗 |
| `capture_region` | 擷取指定的桌面座標矩形 |
| `select_region` | 請使用者在螢幕上拖曳選取範圍 |

擷取類 tool 可設定 `max_dimension`、`format`（`png`／`jpeg`）、`save`、`copy_to_clipboard`、`delay_ms` 等參數。每次擷取後，被擷取的範圍會閃一下橘色邊框提示（不會被拍進截圖），可用環境變數 `RUST_SCREEN_MCP_INDICATOR=0` 關閉。

建議把執行檔複製到固定位置再註冊；MCP 用戶端執行期間會鎖住執行檔，直接指向 `target\release` 會讓之後無法重新編譯。

> **隱私提醒**：連接此伺服器的 AI 助理可以擷取螢幕上任何可見內容，`print_window` 方式甚至能擷取被遮住的視窗。請只在信任的用戶端中啟用，並留意工具核准設定。

## 限制

- 僅支援 Windows 10 / 11。
- 不包含滑鼠游標；受 DRM 保護的影片、系統安全桌面等內容可能無法擷取。
- 視窗截圖會一併拍入遮擋在上方的其他視窗（MCP 的 `print_window` 方式除外）。

## 授權

[MIT](LICENSE)
