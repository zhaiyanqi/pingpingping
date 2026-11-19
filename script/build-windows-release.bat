@echo off
setlocal EnableExtensions DisableDelayedExpansion
rem Build the x64 MSVC executable from any working directory.
rem ZH: 从任意工作目录构建 x64 MSVC 可执行文件。

set "BUILD_PROFILE=release"
set "CARGO_PROFILE=--release"
if /i "%~1"=="--debug" (
    set "BUILD_PROFILE=debug"
    set "CARGO_PROFILE="
    shift
)

set "EXIT_CODE=1"
set "PAUSE_AT_END=1"
if /i "%~1"=="--no-pause" set "PAUSE_AT_END="
if not "%~1"=="" if defined PAUSE_AT_END goto usage
if not "%~2"=="" goto usage

pushd "%~dp0.." || goto finish
set "ROOT_PUSHED=1"
set "RUST_TOOLCHAIN=stable-x86_64-pc-windows-msvc"
set "RUST_TARGET=x86_64-pc-windows-msvc"
where.exe rustup.exe >nul 2>&1
if errorlevel 1 (
    echo Rustup was not found. Install Rust from https://rustup.rs/ and reopen the terminal.
    goto finish
)
rustup run "%RUST_TOOLCHAIN%" rustc --version >nul 2>&1
if errorlevel 1 (
    echo Install the Rust MSVC toolchain with:
    echo   rustup toolchain install %RUST_TOOLCHAIN% --profile minimal
    goto finish
)

rem Use an initialized x64 developer shell, or ask Visual Studio to set one up.
rem 复用已初始化的 x64 开发者终端，或加载 Visual Studio 的编译环境。
if /i "%VSCMD_ARG_TGT_ARCH%"=="x64" (
    where.exe cl.exe >nul 2>&1
    if not errorlevel 1 goto find_fxc
)
set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
set "PPP_VS="
if not exist "%VSWHERE%" goto missing_vs
for /f "usebackq delims=" %%I in (`"%VSWHERE%" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set "PPP_VS=%%I"
if not defined PPP_VS goto missing_vs
if not exist "%PPP_VS%\Common7\Tools\VsDevCmd.bat" goto missing_vs
call "%PPP_VS%\Common7\Tools\VsDevCmd.bat" -no_logo -arch=x64 -host_arch=x64
if errorlevel 1 goto missing_vs
where.exe cl.exe >nul 2>&1
if errorlevel 1 goto missing_vs

:find_fxc
if "%BUILD_PROFILE%"=="debug" goto build
rem GPUI needs the Windows SDK shader compiler for Release builds.
rem ZH: GPUI 的 Release 构建需要 Windows SDK 的着色器编译器。
if defined GPUI_FXC_PATH (
    if exist "%GPUI_FXC_PATH%" goto build
    echo GPUI_FXC_PATH does not point to an existing file.
    goto finish
)
if defined WindowsSdkDir if defined WindowsSDKVersion (
    if exist "%WindowsSdkDir%bin\%WindowsSDKVersion%x64\fxc.exe" set "GPUI_FXC_PATH=%WindowsSdkDir%bin\%WindowsSDKVersion%x64\fxc.exe"
)
if defined GPUI_FXC_PATH goto build
for /f "delims=" %%I in ('where.exe fxc.exe 2^>nul') do if not defined GPUI_FXC_PATH set "GPUI_FXC_PATH=%%I"
if defined GPUI_FXC_PATH goto build
set "PPP_SDK_BIN=%ProgramFiles(x86)%\Windows Kits\10\bin"
for /f "delims=" %%I in ('dir /b /ad /o-n "%PPP_SDK_BIN%\10.*" 2^>nul') do (
    if not defined GPUI_FXC_PATH if exist "%PPP_SDK_BIN%\%%I\x64\fxc.exe" set "GPUI_FXC_PATH=%PPP_SDK_BIN%\%%I\x64\fxc.exe"
)
if not defined GPUI_FXC_PATH (
    echo Install the Windows SDK with Visual Studio Installer, or set GPUI_FXC_PATH to fxc.exe.
    goto finish
)

:build
rustup run "%RUST_TOOLCHAIN%" cargo build %CARGO_PROFILE% --locked --target "%RUST_TARGET%" --target-dir target
set "EXIT_CODE=%ERRORLEVEL%"
if not "%EXIT_CODE%"=="0" goto finish
echo.
echo Built: "%CD%\target\%RUST_TARGET%\%BUILD_PROFILE%\pingpingping.exe"
goto finish

:missing_vs
echo Install the Visual Studio Build Tools workload "Desktop development with C++" and a Windows SDK.
goto finish

:usage
echo Usage: %~nx0 [--debug] [--no-pause]
set "PAUSE_AT_END="
set "EXIT_CODE=2"

:finish
if defined ROOT_PUSHED popd
if not "%EXIT_CODE%"=="0" echo Build failed with exit code %EXIT_CODE%.
if defined PAUSE_AT_END pause
exit /b %EXIT_CODE%
