# goon desktop (Windows)

`goon web` đã là một shell desktop thật: một HTTP server nhỏ chỉ bind loopback, phục vụ UI nhúng
(không bundler, không node_modules, không CDN) và mở bằng cửa sổ Chromium `--app=` nên trông như
app riêng, không có thanh địa chỉ. Tài liệu này nói về phần **packaging** quanh nó: shortcut trên
Desktop và Start Menu, launcher biết tái sử dụng instance đang chạy, và cách gỡ lỗi khi cửa sổ
không mở.

Toàn bộ nằm trong `scripts/desktop/`:

| File | Vai trò |
|---|---|
| `scripts/desktop/install.ps1` | Cài/gỡ shortcut Desktop + Start Menu (`.lnk`), hỗ trợ `-WhatIf` |
| `scripts/desktop/goon-desktop.ps1` | Launcher: tái sử dụng instance đang chạy, ghi state, `-List` / `-Stop` |
| `scripts/desktop/GoonDesktop.Common.ps1` | Hàm dùng chung (tìm `goon.exe`, state file, kiểm tra server sống, mở cửa sổ) |
| `bin/goon-desktop.cmd` | Entry point double-click: forward tham số, `--help`, không treo console khi lỗi |

## Yêu cầu

- **Windows 10 trở lên**, **Windows PowerShell 5.1** — nơi bộ script này được viết và kiểm thật
  trên máy này. PowerShell 7 dùng cùng cú pháp nhưng **chưa được kiểm** (máy này không có `pwsh`).
  Shortcut mặc định gọi `powershell.exe` (5.1), không phụ thuộc `pwsh` có hay không.
- **Chrome hoặc Edge** (bất kỳ bản Chromium nào có `--app=`). Không có thì vẫn mở được nhưng bằng
  trình duyệt mặc định — mất cảm giác "app riêng".
- **`goon.exe`**: bản build từ source (`cargo build --release`) hoặc cài npm
  (`npm install -g @binxgodteli/goon-cli`). Launcher tìm theo thứ tự: `-GoonExe` →
  `PATH` → `target\release` → `target\debug` → `%USERPROFILE%\.local\bin`.
- **Không cần quyền admin.** Mọi thứ ghi vào profile người dùng hiện tại: Desktop, Start Menu,
  `%LOCALAPPDATA%\goon`.

## Cài đặt

Từ checkout `goon-cli`:

```powershell
npm run desktop:install
```

hoặc gọi trực tiếp (đường dẫn đầy đủ, chạy được từ bất kỳ đâu):

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File "C:\Users\Admin\Downloads\goon research\goon-cli\scripts\desktop\install.ps1"
```

Kết quả: hai shortcut mở cùng một thứ.

- `%USERPROFILE%\Desktop\goon desktop.lnk`
- `%APPDATA%\Microsoft\Windows\Start Menu\Programs\goon desktop.lnk`

Script in ra đường dẫn tuyệt đối của từng file đã tạo, `goon.exe` nó tìm thấy, và chế độ shortcut.
Chạy lại lần nữa chỉ **cập nhật** (in `updated`), không tạo file trùng. Gỡ bằng:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File "...\scripts\desktop\install.ps1" -Uninstall
```

### Tuỳ chọn hữu ích

```powershell
# Mở một dự án khác, tên shortcut riêng
... install.ps1 -ProjectDir "C:\work\api" -Name "goon desktop (api)"

# Xem trước, không ghi gì
... install.ps1 -Name "goon desktop (api)" -WhatIf

# Chỉ Desktop, bỏ Start Menu
... install.ps1 -SkipStartMenu

# Chỉ định binary
... install.ps1 -GoonExe "C:\tools\goon.exe"

# Shortcut "thuần" cmd.exe /c goon.exe web --cwd <project>
... install.ps1 -Direct
```

Shortcut mặc định **không** gọi `goon web` trực tiếp mà gọi launcher, vì launcher mới là thứ biết
tái sử dụng instance đang chạy. `-Direct` tạo shortcut đúng dạng
`cmd.exe /c "<goon.exe>" web --cwd "<project>"` cho ai thích đường tắt tối giản.

`.lnk` được tạo qua COM `WScript.Shell`: target là `powershell.exe` (chế độ mặc định) hoặc
`cmd.exe` (chế độ `-Direct`), working directory là thư mục dự án, icon lấy từ `goon.exe`, và
`WindowStyle = 7` (minimized) để console không bao giờ nhá lên trên cửa sổ app.

## Dùng hằng ngày

Double-click shortcut (hoặc gõ `bin\goon-desktop.cmd` trong terminal). Launcher sẽ:

1. đọc `%LOCALAPPDATA%\goon\desktop.json`;
2. nếu đã có instance **đang sống** cho cùng thư mục dự án (và cùng cổng, nếu bạn chỉ định cổng),
   chỉ mở thêm một cửa sổ trỏ vào instance đó — không spawn server thứ hai;
3. nếu chưa có, spawn `goon web` cho dự án đó, đợi nó báo cổng, rồi ghi **port + pid** vào file state.

Nghĩa là: bấm shortcut lần thứ hai, lần thứ ba… vẫn chỉ có một server; cửa sổ thì mở lại bình
thường (giống các app single-instance khác).

Chạy từ terminal, các tham số hay dùng:

```powershell
bin\goon-desktop.cmd                       # mở chính checkout này
bin\goon-desktop.cmd C:\work\api           # mở dự án đó
bin\goon-desktop.cmd -Port 7788            # cổng cố định
bin\goon-desktop.cmd -NoOpen               # chỉ chạy/reuse server, in URL
bin\goon-desktop.cmd -List                 # liệt kê instance đã ghi + trạng thái
bin\goon-desktop.cmd -Stop                 # tắt instance đã ghi
bin\goon-desktop.cmd --help
```

`-ProjectDir`, `-Port`, `-NoOpen`, `-List`, `-Stop`, `-GoonExe`, `-StateFile`, `-StartupTimeoutSec`
đều được forward nguyên vẹn cho `scripts/desktop/goon-desktop.ps1`.

### Phím tắt trong cửa sổ desktop

Đây là UI web (`src/web/ui.js`), không phải TUI, nên bộ phím khác README phần TUI:

| Phím | Chức năng |
|---|---|
| `Ctrl` + `Enter` | Gửi prompt |
| `Enter` / `Shift` + `Enter` | Kết quả tìm: tiếp / lùi |
| `Shift` + `Enter` | Xuống dòng trong ô nhập |
| `Esc` | Dừng stream đang chạy / đóng lớp phủ |
| `Ctrl` + `K` | Bảng lệnh (palette) |
| `Ctrl` + `F` | Tìm trong transcript |
| `Ctrl` + `L` | Xoá transcript |
| `Ctrl` + `/` hoặc `Ctrl` + `?` | Bảng phím tắt của app |
| `Ctrl` + `,` | Settings (chỉ đọc) |
| `↑` `↓` `Enter` | Di chuyển và chạy lệnh trong palette |
| Kéo–thả | Đính kèm file text ≤ 256 KB |

Phần TUI (`goon` trong terminal) có bộ phím riêng — `Ctrl+T`, `Shift+Tab`, `Alt+.`/`Alt+,`,
`Esc Esc`… xem README mục "Phím tắt chính".

### Tắt app

- Trong UI: `Esc` dừng lượt đang chạy; đóng cửa sổ Chromium chỉ đóng **UI**, server vẫn chạy nền.
- Tắt hẳn: `bin\goon-desktop.cmd -Stop` (hoặc
  `powershell -File scripts\desktop\goon-desktop.ps1 -Stop`).
  Chỉ tắt một dự án: thêm `-ProjectDir "C:\work\api"`. Chỉ tắt một cổng: thêm `-Port 7788`.
- `-Stop` không tìm thấy gì thì in "no running instance recorded … nothing to stop" và thoát `0`;
  entry có PID đã chết được dọn (in `cleaned stale entry`) thay vì báo lỗi.
- Khi instance cuối cùng bị tắt, `desktop.json` được **xoá**, nên "không có file" = "không có
  instance nào đang chạy".

## Chạy nhiều dự án

Mỗi shortcut ứng với một `-ProjectDir`; state file giữ **một entry cho mỗi instance**, nên nhiều
dự án chạy song song được:

```powershell
... install.ps1 -ProjectDir "C:\work\api"   -Name "goon desktop (api)"
... install.ps1 -ProjectDir "C:\work\web"   -Name "goon desktop (web)"
... install.ps1 -ProjectDir "C:\work\infra" -Name "goon desktop (infra)" -Port 7791
```

- Server luôn nhận `--cwd <project>`, nên mỗi instance phục vụ đúng dự án của nó; bấm lại shortcut
  của dự án nào thì reuse instance của dự án đó.
- Cùng một dự án vẫn có thể chạy hai instance nếu bạn chỉ định hai cổng khác nhau (mỗi entry được
  ghi riêng); cổng `0` (mặc định) nghĩa là server tự chọn cổng trống.
- `-List` in ra đủ: trạng thái, cổng, PID, thư mục dự án và URL.

## State, log và profile

| Đường dẫn | Nội dung |
|---|---|
| `%LOCALAPPDATA%\goon\desktop.json` | Danh sách instance: `projectDir`, `projectKey`, `port`, `pid`, `url`, `goonExe`, `logFile`, `startedAt`, `adopted` |
| `%LOCALAPPDATA%\goon\logs\desktop-<hash>.err.log` | stderr của instance (chứa dòng `listening on …` và mọi lỗi) |
| `%LOCALAPPDATA%\goon\logs\desktop-<hash>.out.log` | stdout của instance |
| `%USERPROFILE%\.local\share\goon\desktop-profile` | profile Chromium riêng của cửa sổ app (không lẫn với trình duyệt thường) |

Ghi đè bằng `-StateFile <path>` hoặc biến `GOON_DESKTOP_STATE` (hữu ích khi test, hoặc khi chạy
nhiều user). File state ghi kiểu atomic (temp + replace) và tự dọn; JSON hỏng thì chỉ in warning rồi
coi như rỗng, không chặn khởi động.

## Troubleshooting

**Cửa sổ không mở.** Launcher không tự mở cửa sổ khi spawn: `goon web` tự làm việc đó, và nó in lý
do ra log khi thất bại. Xem `%LOCALAPPDATA%\goon\logs\desktop-*.err.log`. Nguyên nhân thường gặp:
không có Chrome/Edge (fallback sang trình duyệt mặc định, hoặc mở URL bằng tay — URL in ra ở dòng
`running on …`); hoặc cửa sổ mở trên desktop khác/màn hình khác. Kiểm tra nhanh:

```powershell
Get-Content "$env:LOCALAPPDATA\goon\logs\desktop-*.err.log" -Tail 20
bin\goon-desktop.cmd -List
bin\goon-desktop.cmd -NoOpen      # in URL để mở bằng tay
```

**Cổng bận.** `goon web` báo `bind 127.0.0.1:<port>: … (os error 10048)`, launcher in thêm gợi ý và
thoát `1`, không ghi state rác. Cách xử lý: dùng instance đang chạy (bấm lại shortcut), hoặc
`-Stop` trước, hoặc chọn cổng khác `-Port 7900`, hoặc tìm ai đang giữ cổng:

```powershell
netstat -ano | findstr :7788
Get-NetTCPConnection -LocalPort 7788 -State Listen | Select-Object OwningProcess
```

Nếu cổng đang phục vụ đúng goon desktop (ví dụ state file bị xoá tay), launcher **adopt** instance
đó và ghi lại PID thay vì spawn thêm.

**`cargo build` lỗi vì binary bị khoá.** Windows không cho ghi đè file `.exe` đang chạy, nên
`cargo build --release` sẽ fail nếu một instance goon desktop vẫn đang giữ `goon.exe`:

```
error: failed to remove file `target\release\goon.exe`
```

Chạy `bin\goon-desktop.cmd -Stop` (đóng hết instance) rồi build lại. Nếu vẫn còn, kiểm tra
`Get-Process goon`.

**Double-click hiện console rồi tắt ngay.** Đó là hành vi đúng: console bị ẩn/xuống minimized, app
mở trong cửa sổ Chromium. Khi lỗi, shortcut giữ console lại để đọc thông báo; chạy từ terminal sẽ
thấy đầy đủ.

**Shortcut còn trỏ đường dẫn cũ** sau khi di chuyển/đổi tên checkout: chạy lại `install.ps1`
(idempotent, tự cập nhật), rồi `-Uninstall` nếu muốn tên cũ biến mất.

**Chính sách PowerShell chặn script.** Shortcut mặc định đã truyền
`-ExecutionPolicy Bypass` cho tiến trình con, nên không phụ thuộc policy máy. Nếu vẫn bị chặn (GPO
cấm PowerShell), cài bằng `-Direct` để shortcut không đi qua PowerShell nữa.

**`goon desktop` chỉ nghe loopback.** `validate_host` từ chối mọi địa chỉ khác `127.0.0.1` /
`localhost` / `::1`. Đây là shell desktop, không phải server: không truy cập được từ máy khác, và
đó là chủ ý, không phải lỗi cấu hình.

**State file hỏng / PID lạ.** Cứ xoá `%LOCALAPPDATA%\goon\desktop.json`; launcher đọc lại từ đầu.
PID được kiểm tra lại kèm health check `GET /api/meta`, nên PID bị tái sử dụng bởi tiến trình khác
không bị coi là instance của goon.

## Giới hạn (biết trước cho khỏi bất ngờ)

- **Chưa có installer kiểu MSI/EXE**: cài đặt là một script PowerShell tạo 2 file `.lnk`, không có
  registry entry, không có Add/Remove Programs, không có auto-update. Gỡ bằng `-Uninstall`.
- **Chưa có tray icon / khay hệ thống**: đóng cửa sổ là đóng UI, server vẫn chạy cho tới khi `-Stop`
  (hoặc khi đăng xuất). Không có menu "Quit" trong tray.
- **Cần browser Chromium** để có cửa sổ `--app=`; thiếu thì rơi về trình duyệt mặc định (có address
  bar, không phải cửa sổ riêng).
- **Chưa ký số (code signing)**: SmartScreen có thể hỏi khi chạy script tải về từ nơi khác; shortcut
  trỏ file local nên không bị hỏi.
- **`-Stop` là kill cứng**: instance bị `Stop-Process -Force`, nên lượt `goon` con đang chạy dở có
  thể không kịp dọn. Muốn sạch: `Esc` trong UI để dừng lượt trước, rồi `-Stop`.
- **Chỉ Windows**: shortcut `.lnk` + COM là Windows-only. Trên macOS/Linux vẫn dùng `goon web`
  (cửa sổ `--app=` do binary tự mở), không có phần cài đặt này.
- **Không autostart**: không thêm gì vào Startup/Run key; mở app là hành động của người dùng.

## Cho người phát triển

- Chạy từ source: `cargo build --release` rồi `bin\goon-desktop.cmd`.
- Kiểm cú pháp script mà không chạy:

```powershell
[void][ScriptBlock]::Create((Get-Content -Raw "...\scripts\desktop\install.ps1"))
```

- Kiểm tra nhanh không ghi gì: `install.ps1 -WhatIf`; kiểm tra launcher không đụng state thật:
  `goon-desktop.ps1 -StateFile "$env:TEMP\gd.json" -List`.
- Cả hai script `$ErrorActionPreference = 'Stop'`, có comment-based help (`-?`), không hardcode
  user path và không yêu cầu admin. `-Name` chỉ nhận tên file thuần (chặn `<>:"/\|?*` và đường
  dẫn), và đường dẫn shortcut được kiểm lại phải nằm trong Desktop/Start Menu trước khi ghi.
