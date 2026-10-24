// Cooler Master MasterLiquid Atmos II LCD (USB 2516:0222) controller.
// Protocol (reverse engineered from MasterCTRL's CMAgent.exe):
//   - Vendor HID interface MI_00, 1024-byte reports.
//   - Output report 0x02 = 0x04 (via SET_REPORT) enables the host protocol.
//   - Report 0x1E: text command, HTTP-like header + JSON body, framed as
//     [1E][5A][len hi][len lo][text][checksum][5A][00 00 00], len = text + 5,
//     checksum = sum of bytes from the length field to the end of text.
//   - Report 0x1F: data chunks (1000 bytes payload each); flag 1 = realtime
//     JPEG frame, flag 0 = file upload announced by "transport".
//   - Report 0x20: device response, framed like 0x1E.
using System;
using System.Collections.Generic;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using Microsoft.Win32.SafeHandles;

static class Native
{
    [StructLayout(LayoutKind.Sequential)]
    public struct SP_DEVICE_INTERFACE_DATA { public int cbSize; public Guid InterfaceClassGuid; public int Flags; public IntPtr Reserved; }

    [DllImport("hid.dll")] public static extern void HidD_GetHidGuid(out Guid g);
    [DllImport("hid.dll")] public static extern bool HidD_GetFeature(SafeFileHandle h, byte[] b, int len);
    [DllImport("hid.dll")] public static extern bool HidD_SetOutputReport(SafeFileHandle h, byte[] b, int len);
    [DllImport("setupapi.dll", CharSet = CharSet.Unicode)] public static extern IntPtr SetupDiGetClassDevs(ref Guid g, IntPtr e, IntPtr w, int f);
    [DllImport("setupapi.dll")] public static extern bool SetupDiEnumDeviceInterfaces(IntPtr s, IntPtr d, ref Guid g, int i, ref SP_DEVICE_INTERFACE_DATA o);
    [DllImport("setupapi.dll", CharSet = CharSet.Unicode)] public static extern bool SetupDiGetDeviceInterfaceDetail(IntPtr s, ref SP_DEVICE_INTERFACE_DATA d, IntPtr o, int size, out int req, IntPtr di);
    [DllImport("setupapi.dll")] public static extern bool SetupDiDestroyDeviceInfoList(IntPtr s);
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    public static extern SafeFileHandle CreateFile(string n, uint access, uint share, IntPtr sa, uint disp, uint flags, IntPtr t);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool WriteFile(SafeFileHandle h, byte[] b, int len, out int written, IntPtr ov);

    public static List<string> HidPaths()
    {
        var result = new List<string>();
        Guid g; HidD_GetHidGuid(out g);
        IntPtr set = SetupDiGetClassDevs(ref g, IntPtr.Zero, IntPtr.Zero, 0x12); // PRESENT | DEVICEINTERFACE
        var d = new SP_DEVICE_INTERFACE_DATA(); d.cbSize = Marshal.SizeOf(d);
        for (int i = 0; SetupDiEnumDeviceInterfaces(set, IntPtr.Zero, ref g, i, ref d); i++)
        {
            int req; SetupDiGetDeviceInterfaceDetail(set, ref d, IntPtr.Zero, 0, out req, IntPtr.Zero);
            IntPtr buf = Marshal.AllocHGlobal(req);
            Marshal.WriteInt32(buf, IntPtr.Size == 8 ? 8 : 6);
            if (SetupDiGetDeviceInterfaceDetail(set, ref d, buf, req, out req, IntPtr.Zero))
                result.Add(Marshal.PtrToStringUni(buf + 4));
            Marshal.FreeHGlobal(buf);
        }
        SetupDiDestroyDeviceInfoList(set);
        return result;
    }
}

class AtmosDevice : IDisposable
{
    const int ReportLen = 1024;
    const byte CmdReport = 0x1E, DataReport = 0x1F, RespReport = 0x20;
    const int ChunkLen = 1000;

    readonly SafeFileHandle writeHandle;
    readonly FileStream reader;
    readonly Thread readerThread;
    readonly System.Collections.Concurrent.BlockingCollection<byte[]> inbox =
        new System.Collections.Concurrent.BlockingCollection<byte[]>();
    readonly Random rng = new Random();
    volatile bool closing;

    // Writes and reads use separate synchronous handles: a blocking read on a
    // shared handle would stall writes. Each HID handle gets its own input queue.
    AtmosDevice(string path)
    {
        writeHandle = OpenPath(path);
        reader = new FileStream(OpenPath(path), FileAccess.Read, ReportLen, false);
        readerThread = new Thread(ReadLoop) { IsBackground = true };
        readerThread.Start();
    }

    static SafeFileHandle OpenPath(string path)
    {
        var h = Native.CreateFile(path, 0xC0000000, 3, IntPtr.Zero, 3, 0, IntPtr.Zero);
        if (h.IsInvalid) throw new IOException("cannot open " + path + " (error " + Marshal.GetLastWin32Error() + "). Is MasterCTRL running?");
        return h;
    }

    public static AtmosDevice Open()
    {
        foreach (var p in Native.HidPaths())
        {
            string lp = p.ToLowerInvariant();
            if (!lp.Contains("vid_2516&pid_0222") || !lp.Contains("mi_00")) continue;
            var dev = new AtmosDevice(p);
            dev.EnableHostProtocol();
            return dev;
        }
        throw new IOException("Atmos II LCD (2516:0222) not found");
    }

    public void Dispose()
    {
        closing = true;
        reader.Dispose();
        writeHandle.Dispose();
    }

    void ReadLoop()
    {
        try
        {
            while (!closing)
            {
                var buf = new byte[ReportLen];
                if (reader.Read(buf, 0, ReportLen) > 0) inbox.Add(buf);
            }
        }
        catch (Exception) { if (!closing) throw; }
    }

    void EnableHostProtocol()
    {
        if (!Native.HidD_SetOutputReport(writeHandle, new byte[] { 0x02, 0x04 }, 2))
            throw new IOException("protocol handshake failed (error " + Marshal.GetLastWin32Error() + ")");
    }

    public byte[] GetFeature(byte id, int len)
    {
        var b = new byte[len + 1];
        b[0] = id;
        if (!Native.HidD_GetFeature(writeHandle, b, b.Length))
            throw new IOException("GetFeature 0x" + id.ToString("X2") + " failed (error " + Marshal.GetLastWin32Error() + ")");
        return b;
    }

    void WriteReport(byte[] report)
    {
        int n;
        if (!Native.WriteFile(writeHandle, report, ReportLen, out n, IntPtr.Zero) || n != ReportLen) throw new IOException("HID write failed (error " + Marshal.GetLastWin32Error() + ")");
    }

    public string ReadResponse(int timeoutMs)
    {
        byte[] buf;
        if (!inbox.TryTake(out buf, timeoutMs)) return null;
        if (buf[0] != RespReport || buf[1] != 0x5A) return "<unexpected report: " + Hex(buf, 16) + ">";
        // The length field is text + 5-byte trailer. The firmware also inserts a stray
        // control byte every 128 body bytes that no length field counts, so skip those.
        int want = ((buf[2] << 8) | buf[3]) - 5;
        var text = new List<byte>();
        for (int i = 4; i < ReportLen && text.Count < want; i++)
        {
            byte b = buf[i];
            if (b >= 0x20 || b == '\r' || b == '\n') text.Add(b);
        }
        return Encoding.UTF8.GetString(text.ToArray());
    }

    public static string Hex(byte[] b, int n)
    {
        return BitConverter.ToString(b, 0, n).Replace("-", " ");
    }

    public void DrainResponses()
    {
        byte[] ignored;
        while (inbox.TryTake(out ignored)) { }
    }

    void SendText(string text)
    {
        byte[] body = Encoding.UTF8.GetBytes(text);
        if (body.Length > ReportLen - 9) throw new ArgumentException("command too long");
        int len = body.Length + 5;
        var report = new byte[ReportLen];
        report[0] = CmdReport;
        report[1] = 0x5A;
        report[2] = (byte)(len >> 8); report[3] = (byte)len;
        Buffer.BlockCopy(body, 0, report, 4, body.Length);
        int sum = 0;
        for (int i = 2; i < 4 + body.Length; i++) sum += report[i];
        report[4 + body.Length] = (byte)sum;
        report[5 + body.Length] = 0x5A;
        WriteReport(report);
    }

    public string Request(string name, string json, int seq, int timeoutMs = 2000, string method = "POST")
    {
        long now = (long)(DateTime.UtcNow - new DateTime(1970, 1, 1)).TotalSeconds;
        DrainResponses();
        SendText(method + " " + name + " 1\r\nSeqNumber=" + seq + "\r\nDate=" + now +
                 "\r\nContentType=json\r\nContentLength=" + Encoding.UTF8.GetByteCount(json) + "\r\n\r\n" + json);
        return ReadResponse(timeoutMs);
    }

    // MasterCTRL repeats this every 4 s while streaming (device keepAliveTimeout is 5 s).
    public void KeepRealtimeAlive()
    {
        long ms = (long)(DateTime.UtcNow - new DateTime(1970, 1, 1)).TotalMilliseconds;
        Request("timestamp", "{\"timestamp\":" + ms + "}", 42, 1000, "STATE");
        Request("realtimeDisplay", "{\"enable\":true}", 42, 1000);
    }

    // Returns the device's current parameters (brightness, rotation, ...).
    public string GetParams(int timeoutMs = 2000)
    {
        DrainResponses();
        SendText("POST param 1\r\n");
        return ReadResponse(timeoutMs);
    }

    // flag 1: realtime JPEG frame; flag 0: file announced by a "transport" command.
    public void SendData(byte[] data, byte flag)
    {
        int total = (data.Length + ChunkLen - 1) / ChunkLen;
        byte session = (byte)rng.Next(0, 60);
        for (int i = 0; i < total; i++)
        {
            int off = i * ChunkLen;
            int len = Math.Min(ChunkLen, data.Length - off);
            int field = len + 0x14;
            var r = new byte[ReportLen];
            r[0] = DataReport;
            r[1] = 0x5C;
            r[2] = (byte)(field >> 8); r[3] = (byte)field;
            r[4] = session;
            r[5] = (byte)(total >> 8); r[6] = (byte)total;
            r[7] = (byte)(i >> 8); r[8] = (byte)i;
            r[9] = flag;
            Buffer.BlockCopy(data, off, r, 24, len);
            WriteReport(r);
        }
    }
}

static class Program
{
    static void Usage()
    {
        Console.WriteLine("AtmosLcd.exe cmd <name> <json> [seq]   send a raw command and print the reply");
        Console.WriteLine("AtmosLcd.exe param                       print current device parameters");
        Console.WriteLine("AtmosLcd.exe brightness <0-100>");
        Console.WriteLine("AtmosLcd.exe rotate <0|90|180|270>");
        Console.WriteLine("AtmosLcd.exe listen [seconds]             print unsolicited reports");
        Console.WriteLine("AtmosLcd.exe show <480x480.jpg> [seconds] stream a JPEG in realtime mode");
    }

    static int Main(string[] args)
    {
        if (args.Length == 0) { Usage(); return 1; }
        try
        {
            using (var dev = AtmosDevice.Open())
            {
                switch (args[0])
                {
                    case "cmd":
                        Print(dev.Request(args[1], args[2], args.Length > 3 ? int.Parse(args[3]) : 0x2c));
                        break;
                    case "param":
                        Print(dev.GetParams());
                        break;
                    case "brightness":
                        Print(dev.Request("brightness", "{\"value\":" + int.Parse(args[1]) + "}", 0x2c));
                        break;
                    case "rotate":
                        Print(dev.Request("rotate", "{\"degree\":" + int.Parse(args[1]) + "}", 0x2d));
                        break;
                    case "listen":
                        var until = DateTime.Now.AddSeconds(args.Length > 1 ? int.Parse(args[1]) : 5);
                        while (DateTime.Now < until) { var r = dev.ReadResponse(200); if (r != null) Print(r); }
                        break;
                    case "show":
                        Show(dev, File.ReadAllBytes(args[1]), args.Length > 2 ? int.Parse(args[2]) : 30);
                        break;
                    default: Usage(); return 1;
                }
            }
            return 0;
        }
        catch (Exception e) { Console.Error.WriteLine("error: " + e.Message); return 2; }
    }

    static void Show(AtmosDevice dev, byte[] jpeg, int seconds)
    {
        var until = DateTime.Now.AddSeconds(seconds);
        var nextKeepAlive = DateTime.MinValue;
        while (DateTime.Now < until)
        {
            if (DateTime.Now >= nextKeepAlive)
            {
                dev.KeepRealtimeAlive();
                nextKeepAlive = DateTime.Now.AddSeconds(4);
            }
            dev.SendData(jpeg, 1);
            Thread.Sleep(200);
        }
    }

    static void Print(string reply)
    {
        Console.WriteLine(reply == null ? "<no reply>" : reply.Replace("\r\n", "\n"));
    }
}
