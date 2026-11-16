# LayoutHijack
LayoutHijack is a lightweight portable system tray utility for Windows 10/11 written in Rust that fixes the common keyboard layout switching bug by replacing the default OS switcher on the fly. Instead of waiting for Windows to fail, it intercepts Shift+Alt and Alt+Shift via a low-level keyboard hook, suppresses native OS switching and menu bar activation using a synthetic 0xFF keystroke, and directly switches the layout of the active control or console window (conhost) with built-in verification, 150 ms switch debouncing, dynamic EN/RU tray icons, and daily logging.

LayoutHijack - портативная утилита в системном трее на Rust, которая полностью устраняет баг Windows 10/11 с игнорированием переключения раскладки по Shift+Alt и Alt+Shift. Программа сохраняет штатные настройки реестра (после её закрытия стандартный переключатель Windows продолжает работать) и перехватывает управление на лету через низкоуровневый хук WH_KEYBOARD_LL. В момент отпускания модификаторов утилита синхронно вбрасывает пустой виртуальный код 0xFF, который гасит нативную смену языка и паразитную активацию верхнего меню окна, после чего в отдельном рабочем потоке вычисляет точный элемент фокуса (GetGUIThreadInfo) или реальный поток консоли через ImmGetDefaultIMEWnd, переключает раскладку запросом WM_INPUTLANGCHANGEREQUEST и верифицирует смену HKL (с автоматическим фоллбэком на корневое окно). Утилита автоматически пропускает сочетания с третьей клавишей, отображает текущий язык (EN/RU) в трее, включает настраиваемый антидребезг (кулдаун 150 мс) и ведёт ежедневные логи в папке logs/.

## Требования для сборки
Windows 10/11
Rust (Cargo)
MSVC toolchain (Visual Studio C++ Build Tools)

## Сборка
```bash
cargo build --release
```
