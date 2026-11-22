# Experiment: does Windows keep the AW2726DM in its desktop topology when the
# monitor's active input is switched away from DisplayPort via DDC/CI?
# Also tests whether Windows can switch the monitor BACK while DP is inactive.
# Safe: always attempts to restore DP1 (0x0F), with retries.
param([int]$AwaySeconds = 6, [int]$Target = 0x11, [int]$HomeInput = 0x0F)

$src = @'
using System; using System.Runtime.InteropServices; using System.Collections.Generic; using System.Text;
public static class Ddc {
  [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] public struct PHYSICAL_MONITOR { public IntPtr h; [MarshalAs(UnmanagedType.ByValTStr, SizeConst=128)] public string desc; }
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int l,t,r,b; }
  public delegate bool MonEnum(IntPtr hMon, IntPtr hdc, ref RECT r, IntPtr d);
  public delegate bool WndEnum(IntPtr h, IntPtr p);
  [DllImport("user32.dll")] public static extern bool EnumDisplayMonitors(IntPtr hdc, IntPtr clip, MonEnum cb, IntPtr d);
  [DllImport("user32.dll")] public static extern bool EnumWindows(WndEnum cb, IntPtr p);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("dxva2.dll")] public static extern bool GetNumberOfPhysicalMonitorsFromHMONITOR(IntPtr h, out uint n);
  [DllImport("dxva2.dll")] public static extern bool GetPhysicalMonitorsFromHMONITOR(IntPtr h, uint n, [Out] PHYSICAL_MONITOR[] a);
  [DllImport("dxva2.dll", SetLastError=true)] public static extern bool GetVCPFeatureAndVCPFeatureReply(IntPtr h, byte code, IntPtr type, out uint cur, out uint max);
  [DllImport("dxva2.dll", SetLastError=true)] public static extern bool SetVCPFeature(IntPtr h, byte code, uint val);
  [DllImport("dxva2.dll")] public static extern bool DestroyPhysicalMonitors(uint n, PHYSICAL_MONITOR[] a);

  public static string Topology() { var o = new List<string>();
    EnumDisplayMonitors(IntPtr.Zero, IntPtr.Zero, (IntPtr m, IntPtr dc, ref RECT r, IntPtr d) => { o.Add(String.Format("({0},{1})-({2},{3})", r.l,r.t,r.r,r.b)); return true; }, IntPtr.Zero);
    return String.Join(" ", o); }
  public static List<string> Windows() { var o = new List<string>();
    EnumWindows((h, p) => { if (!IsWindowVisible(h)) return true; var sb = new StringBuilder(256); GetWindowText(h, sb, 256);
      if (sb.Length == 0) return true; RECT r; GetWindowRect(h, out r); o.Add(String.Format("{0}|({1},{2})-({3},{4})", sb, r.l,r.t,r.r,r.b)); return true; }, IntPtr.Zero);
    return o; }
  // Finds the AW2726DM physical monitor handle (caller must not destroy while using).
  public static IntPtr FindAw(out string desc) { IntPtr found = new IntPtr(-1); string d0 = null;
    EnumDisplayMonitors(IntPtr.Zero, IntPtr.Zero, (IntPtr m, IntPtr dc, ref RECT r, IntPtr d) => {
      uint n; GetNumberOfPhysicalMonitorsFromHMONITOR(m, out n); var a = new PHYSICAL_MONITOR[n]; GetPhysicalMonitorsFromHMONITOR(m, n, a);
      foreach (var p in a) if (p.desc.Contains("AW2726") && found == new IntPtr(-1)) { found = p.h; d0 = p.desc; }
      return true; }, IntPtr.Zero);
    desc = d0; return found; }
  public static string Get(IntPtr h) { uint c, m; bool ok = GetVCPFeatureAndVCPFeatureReply(h, 0x60, IntPtr.Zero, out c, out m);
    return ok ? String.Format("0x{0:X4}", c) : ("ERR " + Marshal.GetLastWin32Error()); }
}
'@
Add-Type -TypeDefinition $src
$log = @()
function L($m) { $s = "{0:HH:mm:ss.fff} {1}" -f (Get-Date), $m; Write-Output $s; $script:log += $s }

$desc = $null; $h = [Ddc]::FindAw([ref]$desc)
if ($h -eq [IntPtr](-1)) { L "AW2726DM not found"; exit 1 }
L "monitor: $desc  input=$([Ddc]::Get($h))"
$topo0 = [Ddc]::Topology(); L "topology before: $topo0"
$win0 = [Ddc]::Windows()

$sw = [Diagnostics.Stopwatch]::StartNew()
$ok = [Ddc]::SetVCPFeature($h, 0x60, [uint32]$Target)
L ("SetVCP 0x60=0x{0:X2} ok={1} took={2}ms" -f $Target, $ok, $sw.ElapsedMilliseconds)

$end = $AwaySeconds * 1000; $last = $topo0
while ($sw.ElapsedMilliseconds -lt $end) {
  Start-Sleep -Milliseconds 250
  $t = [Ddc]::Topology()
  if ($t -ne $last) { L "topology changed @$($sw.ElapsedMilliseconds)ms: $t"; $last = $t }
}
L "input while away (read from Windows): $([Ddc]::Get($h))"

# Restore. The physical monitor handle may be stale if a hotplug happened; re-find.
$restored = $false
for ($i = 0; $i -lt 10 -and -not $restored; $i++) {
  $h2 = [Ddc]::FindAw([ref]$desc); if ($h2 -eq [IntPtr](-1)) { $h2 = $h }
  $t0 = $sw.ElapsedMilliseconds
  $ok = [Ddc]::SetVCPFeature($h2, 0x60, [uint32]$HomeInput)
  L ("restore attempt {0}: SetVCP ok={1} took={2}ms" -f $i, $ok, ($sw.ElapsedMilliseconds - $t0))
  Start-Sleep -Milliseconds 1500
  $cur = [Ddc]::Get($h2); L "readback: $cur"
  if ($cur -match '0x..0F|0x0F0F|0x000F') { $restored = $true }
}
$tEnd = $sw.ElapsedMilliseconds + 8000
while ($sw.ElapsedMilliseconds -lt $tEnd) { Start-Sleep -Milliseconds 250; $t = [Ddc]::Topology(); if ($t -ne $last) { L "topology changed: $t"; $last = $t } }
L "topology after: $last  restored=$restored"
$win1 = [Ddc]::Windows()
$moved = Compare-Object $win0 $win1 | Where-Object SideIndicator -eq '=>'
L ("windows whose rect changed: {0}" -f ($moved | Measure-Object).Count)
$moved | ForEach-Object { L ("  " + $_.InputObject) }
$log | Set-Content "$PSScriptRoot\result-$(Get-Date -Format yyyyMMdd-HHmmss).log"
