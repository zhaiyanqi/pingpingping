@echo off
setlocal EnableExtensions DisableDelayedExpansion
rem Keep settings, history, and custom emotes beside the portable executable.
rem 将便携版的设置、历史和自定义表情保存在程序旁边。
set "PINGPINGPING_DATA_DIR=%~dp0data"
start "" "%~dp0pingpingping.exe"
