using System;
using System.Diagnostics;
using System.Globalization;
using System.IO;
using System.Text;
using System.Threading;

internal static class WindowsProcessAuditTestChild
{
    private static int Main(string[] args)
    {
        if (args.Length == 0)
        {
            Console.Error.WriteLine("missing test mode");
            return 64;
        }

        switch (args[0])
        {
            case "echo-contract":
                Console.WriteLine("cwd=" + Encode(Environment.CurrentDirectory));
                Console.WriteLine("env=" + Encode(Environment.GetEnvironmentVariable("PROCESS_AUDIT_TEST") ?? string.Empty));
                for (int index = 1; index < args.Length; index++)
                {
                    Console.WriteLine("arg=" + Encode(args[index]));
                }

                return 0;
            case "spawn-where":
                return SpawnWhere();
            case "exit":
                return int.Parse(args[1], CultureInfo.InvariantCulture);
            case "sleep":
                Thread.Sleep(int.Parse(args[1], CultureInfo.InvariantCulture));
                return 0;
            case "flood":
                Flood(int.Parse(args[1], CultureInfo.InvariantCulture));
                return 0;
            default:
                Console.Error.WriteLine("unknown test mode: " + args[0]);
                return 64;
        }
    }

    private static string Encode(string value)
    {
        return Convert.ToBase64String(Encoding.UTF8.GetBytes(value));
    }

    private static int SpawnWhere()
    {
        string where = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.System), "where.exe");
        ProcessStartInfo startInfo = new ProcessStartInfo(where, "cmd.exe");
        startInfo.UseShellExecute = false;
        startInfo.CreateNoWindow = true;
        using (Process child = Process.Start(startInfo))
        {
            child.WaitForExit();
            return child.ExitCode;
        }
    }

    private static void Flood(int byteCount)
    {
        byte[] buffer = new byte[8192];
        for (int index = 0; index < buffer.Length; index++)
        {
            buffer[index] = (byte)'x';
        }

        Stream stdout = Console.OpenStandardOutput();
        Stream stderr = Console.OpenStandardError();
        int remaining = byteCount;
        while (remaining > 0)
        {
            int count = Math.Min(remaining, buffer.Length);
            stdout.Write(buffer, 0, count);
            stderr.Write(buffer, 0, count);
            remaining -= count;
        }

        stdout.Flush();
        stderr.Flush();
    }
}
