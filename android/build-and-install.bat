@echo off
REM Builds the PhoneCast phone app and installs it on the USB-connected phone.
setlocal
cd /d "%~dp0"
if not exist D:\tmp\jdk mkdir D:\tmp\jdk
REM Java on Windows fails with temp folders that contain spaces.
set TMP=D:\tmp\jdk
set TEMP=D:\tmp\jdk
if "%JAVA_HOME%"=="" for /d %%J in ("C:\Program Files\Eclipse Adoptium\jdk-17*") do set "JAVA_HOME=%%J"

call "%~dp0gradlew.bat" -p "%~dp0." assembleRelease || goto :fail
"%LOCALAPPDATA%\Android\Sdk\platform-tools\adb.exe" install -r "%~dp0app\build\outputs\apk\release\app-release.apk" || goto :fail
echo.
echo Installed. Open "PhoneCast" on your phone.
goto :eof

:fail
echo.
echo Build or install failed - see the messages above.
pause
exit /b 1
