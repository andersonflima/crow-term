// Windows automation helper for the GUI journeys (tests/gui/drivers/windows.py).
// Built on the runner with the .NET Framework csc (C# 5 syntax only).
// Input goes through SendInput, i.e. the same queue as a real keyboard/mouse.
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Windows.Forms;

static class WinGui
{
    [StructLayout(LayoutKind.Sequential)]
    struct RECT { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)]
    struct POINT { public int X, Y; }
    [StructLayout(LayoutKind.Sequential)]
    struct MOUSEINPUT { public int dx, dy; public uint mouseData, dwFlags, time; public IntPtr dwExtraInfo; }
    [StructLayout(LayoutKind.Sequential)]
    struct KEYBDINPUT { public ushort wVk, wScan; public uint dwFlags, time; public IntPtr dwExtraInfo; }
    [StructLayout(LayoutKind.Explicit)]
    struct INPUTUNION {
        [FieldOffset(0)] public MOUSEINPUT mi;
        [FieldOffset(0)] public KEYBDINPUT ki;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct INPUT { public uint type; public INPUTUNION u; }

    delegate bool EnumProc(IntPtr hwnd, IntPtr lparam);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc proc, IntPtr lparam);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll")] static extern bool SetForegroundWindow(IntPtr hwnd);
    [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] static extern bool BringWindowToTop(IntPtr hwnd);
    [DllImport("user32.dll")] static extern bool ShowWindow(IntPtr hwnd, int cmd);
    [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr hwnd, out RECT rect);
    [DllImport("user32.dll")] static extern bool GetClientRect(IntPtr hwnd, out RECT rect);
    [DllImport("user32.dll")] static extern bool ClientToScreen(IntPtr hwnd, ref POINT point);
    [DllImport("user32.dll")] static extern bool SetWindowPos(IntPtr hwnd, IntPtr after, int x, int y, int w, int h, uint flags);
    [DllImport("user32.dll")] static extern bool PostMessage(IntPtr hwnd, uint msg, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] static extern uint SendInput(uint count, INPUT[] inputs, int size);
    [DllImport("user32.dll")] static extern short VkKeyScan(char c);
    [DllImport("user32.dll")] static extern uint MapVirtualKey(uint code, uint mapType);
    [DllImport("user32.dll")] static extern bool SetProcessDpiAwarenessContext(IntPtr value);
    [DllImport("user32.dll")] static extern int GetSystemMetrics(int index);
    [DllImport("user32.dll")] static extern bool AllowSetForegroundWindow(int pid);

    const uint INPUT_MOUSE = 0, INPUT_KEYBOARD = 1;
    const uint KEYEVENTF_EXTENDEDKEY = 1, KEYEVENTF_KEYUP = 2;
    const uint MOUSEEVENTF_LEFTDOWN = 2, MOUSEEVENTF_LEFTUP = 4;

    static int Main(string[] args)
    {
        // Per-monitor DPI awareness v2: every coordinate below is physical pixels.
        try { SetProcessDpiAwarenessContext(new IntPtr(-4)); } catch (Exception) { }
        try
        {
            switch (args[0])
            {
                case "find": return Find(uint.Parse(args[1]));
                case "activate": Activate(Hwnd(args[1])); return 0;
                case "foreground": Console.WriteLine(GetForegroundWindow().ToInt64()); return 0;
                case "key": Key(args[1]); return 0;
                case "type": TypeText(Encoding.UTF8.GetString(Convert.FromBase64String(args[1]))); return 0;
                case "click": Click(Hwnd(args[1]), int.Parse(args[2]), int.Parse(args[3])); return 0;
                case "move": Move(Hwnd(args[1]), int.Parse(args[2]), int.Parse(args[3])); return 0;
                case "keydown": SendKeys(new[] { Vk(args[1]) }, true); return 0;
                case "keyup": SendKeys(new[] { Vk(args[1]) }, false); return 0;
                case "resize": Resize(Hwnd(args[1]), int.Parse(args[2]), int.Parse(args[3])); return 0;
                case "close": PostMessage(Hwnd(args[1]), 0x0010, IntPtr.Zero, IntPtr.Zero); return 0;
                case "screenshot": Screenshot(args[1]); return 0;
                case "clipboard": return Clip(Encoding.UTF8.GetString(Convert.FromBase64String(args[1])));
                // Large payloads: a command line is capped at 32K characters.
                case "clipboard-file": return Clip(System.IO.File.ReadAllText(args[1], Encoding.UTF8));
                case "sink": Sink(); return 0;
                case "info": Info(); return 0;
            }
        }
        catch (Exception error)
        {
            Console.Error.WriteLine(error.ToString());
            return 2;
        }
        Console.Error.WriteLine("unknown command");
        return 64;
    }

    static IntPtr Hwnd(string text) { return new IntPtr(long.Parse(text)); }

    static int Find(uint pid)
    {
        var found = new List<IntPtr>();
        EnumWindows(delegate (IntPtr hwnd, IntPtr l) {
            uint owner;
            GetWindowThreadProcessId(hwnd, out owner);
            if (owner == pid && IsWindowVisible(hwnd)) found.Add(hwnd);
            return true;
        }, IntPtr.Zero);
        foreach (var hwnd in found) Console.WriteLine(hwnd.ToInt64());
        return found.Count > 0 ? 0 : 1;
    }

    static void Activate(IntPtr hwnd)
    {
        // Already in front: no Alt tap (it would reach the window as a key press).
        if (GetForegroundWindow() == hwnd) return;
        // A tap of Alt makes this process the last input source, which lets
        // SetForegroundWindow succeed from a background process.
        SendKeys(new ushort[] { 0x12 }, true);
        SendKeys(new ushort[] { 0x12 }, false);
        ShowWindow(hwnd, 5);
        BringWindowToTop(hwnd);
        SetForegroundWindow(hwnd);
        for (int i = 0; i < 40 && GetForegroundWindow() != hwnd; i++) { Thread.Sleep(25); SetForegroundWindow(hwnd); }
        if (GetForegroundWindow() != hwnd) throw new Exception("could not focus window " + hwnd.ToInt64());
    }

    static INPUT KeyInput(ushort vk, bool down)
    {
        var input = new INPUT();
        input.type = INPUT_KEYBOARD;
        input.u.ki.wVk = vk;
        input.u.ki.wScan = (ushort)MapVirtualKey(vk, 0);
        uint flags = down ? 0 : KEYEVENTF_KEYUP;
        // Arrows/navigation keys and right-hand modifiers are extended keys.
        if (vk >= 0x21 && vk <= 0x2E) flags |= KEYEVENTF_EXTENDEDKEY;
        input.u.ki.dwFlags = flags;
        return input;
    }

    static void SendKeys(ushort[] keys, bool down)
    {
        var inputs = new INPUT[keys.Length];
        for (int i = 0; i < keys.Length; i++) inputs[i] = KeyInput(keys[i], down);
        SendInput((uint)inputs.Length, inputs, Marshal.SizeOf(typeof(INPUT)));
    }

    static ushort Vk(string name)
    {
        switch (name)
        {
            case "ctrl": return 0x11;
            case "shift": return 0x10;
            case "alt": return 0x12;
            case "Return": return 0x0D;
            case "Escape": return 0x1B;
            case "Tab": return 0x09;
            case "slash": return (ushort)(VkKeyScan('/') & 0xFF);
        }
        if (name.Length == 1) return (ushort)(VkKeyScan(name[0]) & 0xFF);
        throw new Exception("unknown key " + name);
    }

    static void Key(string chord)
    {
        var parts = chord.Split('+');
        var keys = new List<ushort>();
        foreach (var part in parts) keys.Add(Vk(part));
        // Press in order, release in reverse order, one SendInput batch each.
        SendKeys(keys.ToArray(), true);
        Thread.Sleep(20);
        keys.Reverse();
        SendKeys(keys.ToArray(), false);
        Thread.Sleep(30);
    }

    static void TypeText(string text)
    {
        foreach (char c in text)
        {
            short scan = VkKeyScan(c);
            if (scan == -1) throw new Exception("cannot type " + c);
            var keys = new List<ushort>();
            if ((scan & 0x100) != 0) keys.Add(0x10);
            if ((scan & 0x200) != 0) keys.Add(0x11);
            if ((scan & 0x400) != 0) keys.Add(0x12);
            keys.Add((ushort)(scan & 0xFF));
            SendKeys(keys.ToArray(), true);
            keys.Reverse();
            SendKeys(keys.ToArray(), false);
            Thread.Sleep(30);
        }
    }

    static void Move(IntPtr hwnd, int x, int y)
    {
        var point = new POINT();
        point.X = x; point.Y = y;
        ClientToScreen(hwnd, ref point);
        SetCursorPos(point.X, point.Y);
        Thread.Sleep(60);
    }

    static void Click(IntPtr hwnd, int x, int y)
    {
        Move(hwnd, x, y);
        var inputs = new INPUT[2];
        inputs[0].type = INPUT_MOUSE; inputs[0].u.mi.dwFlags = MOUSEEVENTF_LEFTDOWN;
        inputs[1].type = INPUT_MOUSE; inputs[1].u.mi.dwFlags = MOUSEEVENTF_LEFTUP;
        SendInput(2, inputs, Marshal.SizeOf(typeof(INPUT)));
    }

    static void Resize(IntPtr hwnd, int width, int height)
    {
        RECT outer, inner;
        GetWindowRect(hwnd, out outer);
        GetClientRect(hwnd, out inner);
        int dx = (outer.Right - outer.Left) - (inner.Right - inner.Left);
        int dy = (outer.Bottom - outer.Top) - (inner.Bottom - inner.Top);
        SetWindowPos(hwnd, IntPtr.Zero, 0, 0, width + dx, height + dy, 0x0002 | 0x0004 | 0x0010);
    }

    static void Screenshot(string path)
    {
        int x = GetSystemMetrics(76), y = GetSystemMetrics(77);
        int w = GetSystemMetrics(78), h = GetSystemMetrics(79);
        using (var bitmap = new Bitmap(w, h, PixelFormat.Format24bppRgb))
        {
            using (var graphics = Graphics.FromImage(bitmap))
                graphics.CopyFromScreen(x, y, 0, 0, new Size(w, h), CopyPixelOperation.SourceCopy);
            bitmap.Save(path, ImageFormat.Png);
        }
    }

    static int Clip(string text)
    {
        int code = 1;
        var thread = new Thread(delegate () {
            for (int i = 0; i < 20; i++)
            {
                try { Clipboard.SetDataObject(text, true, 5, 100); code = 0; return; }
                catch (ExternalException) { Thread.Sleep(100); }
            }
        });
        thread.SetApartmentState(ApartmentState.STA);
        thread.Start();
        thread.Join();
        return code;
    }

    static void Sink()
    {
        var form = new Form();
        form.Text = "crow focus sink";
        form.StartPosition = FormStartPosition.Manual;
        form.Location = new Point(900, 600);
        form.Size = new Size(240, 120);
        form.ShowInTaskbar = true;
        Application.Run(form);
    }

    static void Info()
    {
        Console.WriteLine("virtual screen " + GetSystemMetrics(78) + "x" + GetSystemMetrics(79) +
            " foreground " + GetForegroundWindow().ToInt64() + " session " + Process.GetCurrentProcess().SessionId +
            " interactive " + Environment.UserInteractive);
    }
}
