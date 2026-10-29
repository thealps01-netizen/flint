# Presses Alt+Space through Windows (keybd_event), as a user would; used by
# gate.mjs next to this file.
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class GateKb {
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
}
"@
$KEYUP = 2
[GateKb]::keybd_event(0x12, 0, 0, [UIntPtr]::Zero)      # Alt down
[GateKb]::keybd_event(0x20, 0, 0, [UIntPtr]::Zero)      # Space down
[GateKb]::keybd_event(0x20, 0, $KEYUP, [UIntPtr]::Zero) # Space up
[GateKb]::keybd_event(0x12, 0, $KEYUP, [UIntPtr]::Zero) # Alt up
