# Runs a program inside a hidden window station. Each window station has its own
# clipboard, so the program can't see or change the user's clipboard, and nothing it
# does appears on the user's screen.
#
# It isn't entirely private. The sandboxed parts of Chromium-based apps, such as
# VS Code, Edge and Teams, use the same station and keep it in existence. If its
# clipboard gets stuck, clipboard tests fail with ClipboardOccupied (OpenClipboard
# gives error 5). Closing all those apps should clear it, since the station goes
# away with its last user.
param([string]$Exe, [string]$Arguments, [string]$Output, [int]$TimeoutSeconds = 60)

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class Isolated {
  [DllImport("user32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
  public static extern IntPtr CreateWindowStationW(string name, uint flags, uint access, IntPtr sa);
  [DllImport("user32.dll", SetLastError=true)]
  public static extern IntPtr GetProcessWindowStation();
  [DllImport("user32.dll", SetLastError=true)]
  public static extern bool SetProcessWindowStation(IntPtr h);
  [DllImport("user32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
  public static extern IntPtr CreateDesktopW(string name, IntPtr device, IntPtr mode, uint flags, uint access, IntPtr sa);
  [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)]
  public struct STARTUPINFO {
    public int cb; public string lpReserved; public string lpDesktop; public string lpTitle;
    public int dwX, dwY, dwXSize, dwYSize, dwXCountChars, dwYCountChars, dwFillAttribute, dwFlags;
    public short wShowWindow, cbReserved2; public IntPtr lpReserved2, hStdInput, hStdOutput, hStdError;
  }
  [StructLayout(LayoutKind.Sequential)]
  public struct PROCESS_INFORMATION { public IntPtr hProcess, hThread; public int dwProcessId, dwThreadId; }
  [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
  public static extern bool CreateProcessW(string app, string cmd, IntPtr pa, IntPtr ta, bool inherit, uint flags, IntPtr env, string cwd, ref STARTUPINFO si, out PROCESS_INFORMATION pi);
  [DllImport("user32.dll", CharSet=CharSet.Unicode, SetLastError=true)] public static extern bool GetUserObjectInformationW(IntPtr h, int index, System.Text.StringBuilder info, int length, out int needed);
  [DllImport("kernel32.dll")] public static extern uint WaitForSingleObject(IntPtr h, uint ms);
  [DllImport("kernel32.dll")] public static extern bool GetExitCodeProcess(IntPtr h, out uint code);
  [DllImport("kernel32.dll")] public static extern bool TerminateProcess(IntPtr h, uint code);

  public static string Run(string exe, string commandLine, string cwd, uint timeoutMs) {
    const uint ALL = 0x37F;           // WINSTA_ALL_ACCESS
    const uint GENERIC_ALL = 0x10000000;
    const uint CREATE_NO_WINDOW = 0x08000000;
    // Only administrators may name a station, so with no name Windows picks one from the logon
    // session. Runs share it for as long as anything is still using it.
    IntPtr station = CreateWindowStationW(null, 0, ALL, IntPtr.Zero);
    if (station == IntPtr.Zero) return "CreateWindowStation failed: " + Marshal.GetLastWin32Error();
    IntPtr original = GetProcessWindowStation();
    if (!SetProcessWindowStation(station)) return "SetProcessWindowStation failed: " + Marshal.GetLastWin32Error();
    IntPtr desktop = CreateDesktopW("Default", IntPtr.Zero, IntPtr.Zero, 0, GENERIC_ALL, IntPtr.Zero);
    int desktopError = Marshal.GetLastWin32Error();
    SetProcessWindowStation(original);
    if (desktop == IntPtr.Zero) return "CreateDesktop failed: " + desktopError;

    STARTUPINFO si = new STARTUPINFO();
    si.cb = Marshal.SizeOf(si);
    System.Text.StringBuilder name = new System.Text.StringBuilder(256);
    int needed;
    GetUserObjectInformationW(station, 2, name, 512, out needed);
    si.lpDesktop = name.ToString() + "\\Default";
    PROCESS_INFORMATION pi;
    if (!CreateProcessW(exe, commandLine, IntPtr.Zero, IntPtr.Zero, false, CREATE_NO_WINDOW, IntPtr.Zero, cwd, ref si, out pi))
      return "CreateProcess failed: " + Marshal.GetLastWin32Error();
    uint waited = WaitForSingleObject(pi.hProcess, timeoutMs);
    if (waited != 0) {
      // This is cmd.exe, so the program it started has to go with it, or it stays behind
      // holding on to its file and that window station's clipboard.
      System.Diagnostics.Process.Start("taskkill.exe", "/F /T /PID " + pi.dwProcessId).WaitForExit();
      TerminateProcess(pi.hProcess, 99);
      return "timed out";
    }
    uint code; GetExitCodeProcess(pi.hProcess, out code);
    return "exit code " + code;
  }
}
'@

# The program can't inherit a redirected console here, so a small batch file writes
# its output to a file.
[IO.File]::WriteAllText($Output, '') # so a run that never starts can't show an older run's results
$batch = [IO.Path]::ChangeExtension($Output, '.cmd')
Set-Content -Path $batch -Encoding Ascii -Value "@`"$Exe`" $Arguments > `"$Output`" 2>&1`r`n@exit /b %errorlevel%"
$cmd = "$env:SystemRoot\System32\cmd.exe"
[Isolated]::Run($cmd, "cmd.exe /c `"$batch`"", (Get-Location).Path, [uint32]($TimeoutSeconds * 1000))
if (Test-Path $Output) { Get-Content $Output | Select-String -Pattern '^test |panicked|assert|left:|right:|the image|writing here|^running' | ForEach-Object { $_.Line } }
