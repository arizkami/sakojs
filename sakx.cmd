@ECHO off
REM Shorthand for `sako x`, in the spirit of npx and bunx.
REM Searches next to this script, then the local build output, then PATH, so it
REM works both from an install and straight out of a checkout.
SETLOCAL
SET "_sako="
IF EXIST "%~dp0sako.exe" SET "_sako=%~dp0sako.exe"
IF NOT DEFINED _sako IF EXIST "%~dp0target\x86_64-pc-windows-msvc\release\sako.exe" SET "_sako=%~dp0target\x86_64-pc-windows-msvc\release\sako.exe"
IF NOT DEFINED _sako IF EXIST "%~dp0target\release\sako.exe" SET "_sako=%~dp0target\release\sako.exe"
IF NOT DEFINED _sako (
  WHERE sako >NUL 2>&1
  IF ERRORLEVEL 1 (
    ECHO sakx: cannot find sako. Build it with `cargo build --release` or put it on PATH.>&2
    EXIT /B 127
  )
  SET "_sako=sako"
)
"%_sako%" x %*
