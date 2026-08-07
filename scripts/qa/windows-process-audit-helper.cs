using System;
using System.Collections;
using System.Collections.Generic;
using System.ComponentModel;
using System.Diagnostics;
using System.Globalization;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using Microsoft.Win32.SafeHandles;

internal static class WindowsProcessAuditHelper
{
    private const int ObserverFailureExitCode = 125;

    private static int Main(string[] args)
    {
#if PROCESS_AUDIT_TESTING
        if (args.Length == 1 && args[0] == "--test-run-internal-unit-tests")
        {
            return AuditRunner.RunInternalUnitTests();
        }
#endif

        Options options;
        try
        {
            options = Options.Parse(args);
        }
        catch (Exception error)
        {
            Console.Error.WriteLine("process-audit argument error: " + error.Message);
            return ObserverFailureExitCode;
        }

        AuditEvidence evidence = new AuditEvidence(options);
        int exitCode = ObserverFailureExitCode;
        try
        {
            exitCode = new AuditRunner(options, evidence).Run();
            evidence.Status = "success";
        }
        catch (Exception error)
        {
            evidence.Status = "failed";
            evidence.ObserverError = error.Message;
            Console.Error.WriteLine("process-audit observer failure: " + error.Message);
            exitCode = ObserverFailureExitCode;
        }

        evidence.FinishedAtUtc = Timestamp();
        try
        {
            EvidenceWriter.Write(options.EvidencePath, evidence);
        }
        catch (Exception error)
        {
            Console.Error.WriteLine("process-audit evidence failure: " + error.Message);
            exitCode = ObserverFailureExitCode;
        }

        return exitCode;
    }

    internal static string Timestamp()
    {
        return DateTime.UtcNow.ToString("o", CultureInfo.InvariantCulture);
    }

    private sealed class Options
    {
        internal string EvidencePath;
        internal int TimeoutMs;
        internal string WorkingDirectory;
        internal string EnvironmentMode;
        internal readonly SortedDictionary<string, string> EnvironmentOverrides =
            new SortedDictionary<string, string>(StringComparer.OrdinalIgnoreCase);
        internal readonly HashSet<string> ForbiddenImages = new HashSet<string>(StringComparer.OrdinalIgnoreCase)
        {
            "whoami.exe",
            "icacls.exe"
        };
        internal string Executable;
        internal readonly List<string> ChildArguments = new List<string>();
#if PROCESS_AUDIT_TESTING
        internal bool TestFailBeforeJobAssignment;
#endif

        internal static Options Parse(string[] args)
        {
            Options options = new Options();
            options.TimeoutMs = 60000;
            options.EnvironmentMode = "inherit";
            int index = 0;
            while (index < args.Length)
            {
                string argument = args[index++];
                if (argument == "--")
                {
                    break;
                }

                switch (argument)
                {
                    case "--evidence":
                        options.EvidencePath = RequireValue(args, ref index, argument);
                        break;
                    case "--timeout-ms":
                        string timeout = RequireValue(args, ref index, argument);
                        if (!int.TryParse(timeout, NumberStyles.None, CultureInfo.InvariantCulture, out options.TimeoutMs) || options.TimeoutMs <= 0)
                        {
                            throw new ArgumentException("--timeout-ms must be a positive 32-bit integer");
                        }

                        break;
                    case "--working-directory":
                        options.WorkingDirectory = RequireValue(args, ref index, argument);
                        break;
                    case "--environment":
                        options.EnvironmentMode = RequireValue(args, ref index, argument).ToLowerInvariant();
                        if (options.EnvironmentMode != "inherit" && options.EnvironmentMode != "clear")
                        {
                            throw new ArgumentException("--environment must be inherit or clear");
                        }

                        break;
                    case "--env":
                        string name = RequireValue(args, ref index, argument);
                        string value = RequireValue(args, ref index, argument);
                        ValidateEnvironmentName(name);
                        if (value.IndexOf('\0') >= 0)
                        {
                            throw new ArgumentException("environment values cannot contain NUL");
                        }

                        options.EnvironmentOverrides[name] = value;
                        break;
                    case "--forbid-image":
                        string image = RequireValue(args, ref index, argument);
                        if (string.IsNullOrEmpty(image) || Path.GetFileName(image) != image)
                        {
                            throw new ArgumentException("--forbid-image requires a basename without directory separators");
                        }

                        options.ForbiddenImages.Add(image);
                        break;
                    case "--child-argument-base64":
                        string encodedArgument = RequireValue(args, ref index, argument);
                        options.ChildArguments.Add(DecodeChildArgument(encodedArgument));
                        break;
#if PROCESS_AUDIT_TESTING
                    case "--test-fail-before-job-assignment":
                        options.TestFailBeforeJobAssignment = true;
                        break;
#endif
                    default:
                        throw new ArgumentException("unknown option before --: " + argument);
                }
            }

            if (index >= args.Length)
            {
                throw new ArgumentException("expected an absolute executable path after --");
            }

            options.Executable = args[index++];
            while (index < args.Length)
            {
                options.ChildArguments.Add(args[index++]);
            }

            if (string.IsNullOrEmpty(options.EvidencePath))
            {
                throw new ArgumentException("--evidence is required");
            }

            if (string.IsNullOrEmpty(options.WorkingDirectory))
            {
                throw new ArgumentException("--working-directory is required");
            }

            options.EvidencePath = Path.GetFullPath(options.EvidencePath);
            options.WorkingDirectory = Path.GetFullPath(options.WorkingDirectory);
            options.Executable = Path.GetFullPath(options.Executable);
            if (!Directory.Exists(options.WorkingDirectory))
            {
                throw new ArgumentException("working directory does not exist: " + options.WorkingDirectory);
            }

            if (!File.Exists(options.Executable))
            {
                throw new ArgumentException("executable does not exist: " + options.Executable);
            }

            string evidenceDirectory = Path.GetDirectoryName(options.EvidencePath);
            if (string.IsNullOrEmpty(evidenceDirectory) || !Directory.Exists(evidenceDirectory))
            {
                throw new ArgumentException("evidence directory does not exist: " + evidenceDirectory);
            }

            return options;
        }

        private static string RequireValue(string[] args, ref int index, string option)
        {
            if (index >= args.Length)
            {
                throw new ArgumentException(option + " requires a value");
            }

            return args[index++];
        }

        private static void ValidateEnvironmentName(string name)
        {
            if (string.IsNullOrEmpty(name) || name.IndexOf('=') >= 0 || name.IndexOf('\0') >= 0)
            {
                throw new ArgumentException("environment names must be nonempty and cannot contain '=' or NUL");
            }
        }

        internal static string DecodeChildArgument(string encodedArgument)
        {
            byte[] argumentBytes;
            try
            {
                argumentBytes = Convert.FromBase64String(encodedArgument);
            }
            catch (FormatException error)
            {
                throw new ArgumentException("--child-argument-base64 requires framed UTF-8 base64", error);
            }

            if (argumentBytes.Length == 0 || argumentBytes[0] != 0)
            {
                throw new ArgumentException("--child-argument-base64 is missing its zero-byte framing prefix");
            }

            string decoded;
            try
            {
                decoded = new UTF8Encoding(false, true).GetString(argumentBytes, 1, argumentBytes.Length - 1);
            }
            catch (DecoderFallbackException error)
            {
                throw new ArgumentException("--child-argument-base64 contains invalid UTF-8", error);
            }

            if (decoded.IndexOf('\0') >= 0)
            {
                throw new ArgumentException("decoded child arguments cannot contain NUL");
            }

            return decoded;
        }
    }

    private sealed class AuditRunner
    {
        private const uint DEBUG_PROCESS = 0x00000001;
        private const uint CREATE_SUSPENDED = 0x00000004;
        private const uint CREATE_UNICODE_ENVIRONMENT = 0x00000400;
        private const uint EXTENDED_STARTUPINFO_PRESENT = 0x00080000;
        private static readonly UIntPtr PROC_THREAD_ATTRIBUTE_HANDLE_LIST = new UIntPtr(0x00020002);
        private const uint StartfUseStdHandles = 0x00000100;
        private const uint DbgContinue = 0x00010002;
        private const uint DbgExceptionNotHandled = 0x80010001;
        private const uint ExceptionBreakpoint = 0x80000003;
        private const uint EXCEPTION_DEBUG_EVENT = 1;
        private const uint CREATE_THREAD_DEBUG_EVENT = 2;
        private const uint CREATE_PROCESS_DEBUG_EVENT = 3;
        private const uint EXIT_THREAD_DEBUG_EVENT = 4;
        private const uint EXIT_PROCESS_DEBUG_EVENT = 5;
        private const uint LOAD_DLL_DEBUG_EVENT = 6;
        private const uint UNLOAD_DLL_DEBUG_EVENT = 7;
        private const uint OUTPUT_DEBUG_STRING_EVENT = 8;
        private const uint RIP_EVENT = 9;
        private const uint ErrorSemTimeout = 121;
        private const uint ErrorInsufficientBuffer = 122;
        private const uint HandleFlagInherit = 0x00000001;
        private const uint GenericRead = 0x80000000;
        private const uint FileShareRead = 0x00000001;
        private const uint FileShareWrite = 0x00000002;
        private const uint OpenExisting = 3;
        private const uint FileAttributeNormal = 0x00000080;
        private const uint JobObjectLimitKillOnJobClose = 0x00002000;
        private const int JobObjectExtendedLimitInformation = 9;
        private const uint InfiniteStillActive = 259;

        private enum DebugEventKind
        {
            Exception,
            CreateThread,
            CreateProcess,
            ExitThread,
            ExitProcess,
            LoadDll,
            UnloadDll,
            OutputDebugString
        }

        private readonly Options options;
        private readonly AuditEvidence evidence;
        private readonly HashSet<uint> active = new HashSet<uint>();
        private readonly HashSet<uint> initialBreakpoints = new HashSet<uint>();
        private readonly Stopwatch stopwatch = new Stopwatch();
        private IntPtr job = IntPtr.Zero;
        private IntPtr rootProcess = IntPtr.Zero;
        private IntPtr rootThread = IntPtr.Zero;
        private uint rootProcessId;
        private bool rootExitSeen;
        private uint rootExitCode;
        private PipePump stdoutPump;
        private PipePump stderrPump;
        private bool rootAssignedToJob;

        internal AuditRunner(Options options, AuditEvidence evidence)
        {
            this.options = options;
            this.evidence = evidence;
        }

#if PROCESS_AUDIT_TESTING
        internal static int RunInternalUnitTests()
        {
            try
            {
                uint[] codes =
                {
                    EXCEPTION_DEBUG_EVENT,
                    CREATE_THREAD_DEBUG_EVENT,
                    CREATE_PROCESS_DEBUG_EVENT,
                    EXIT_THREAD_DEBUG_EVENT,
                    EXIT_PROCESS_DEBUG_EVENT,
                    LOAD_DLL_DEBUG_EVENT,
                    UNLOAD_DLL_DEBUG_EVENT,
                    OUTPUT_DEBUG_STRING_EVENT
                };
                DebugEventKind[] expected =
                {
                    DebugEventKind.Exception,
                    DebugEventKind.CreateThread,
                    DebugEventKind.CreateProcess,
                    DebugEventKind.ExitThread,
                    DebugEventKind.ExitProcess,
                    DebugEventKind.LoadDll,
                    DebugEventKind.UnloadDll,
                    DebugEventKind.OutputDebugString
                };
                for (int index = 0; index < codes.Length; index++)
                {
                    DebugEvent known = new DebugEvent();
                    known.Code = codes[index];
                    DebugEventKind actual = ClassifyDebugEvent(ref known);
                    if (actual != expected[index])
                    {
                        throw new InvalidOperationException(
                            "debug event " + codes[index] + " classified as " + actual +
                            " instead of " + expected[index]);
                    }
                }

                DebugEvent rip = new DebugEvent();
                rip.Code = RIP_EVENT;
                rip.Data.Rip.Error = 17;
                rip.Data.Rip.Type = 23;
                bool ripRejected = false;
                try
                {
                    ClassifyDebugEvent(ref rip);
                }
                catch (InvalidOperationException error)
                {
                    ripRejected = error.Message.Contains("RIP_EVENT") &&
                        error.Message.Contains("error=17") &&
                        error.Message.Contains("type=23");
                }

                if (!ripRejected)
                {
                    throw new InvalidOperationException("RIP_EVENT was not rejected with its RIP_INFO payload");
                }

                DebugEvent unknown = new DebugEvent();
                unknown.Code = 42;
                bool unknownRejected = false;
                try
                {
                    ClassifyDebugEvent(ref unknown);
                }
                catch (InvalidOperationException error)
                {
                    unknownRejected = error.Message.Contains("unknown debug event code: 42");
                }

                if (!unknownRejected)
                {
                    throw new InvalidOperationException("unknown debug event code was not rejected");
                }

                byte[] nulArgument = { 0, (byte)'a', 0, (byte)'b' };
                bool nulRejected = false;
                try
                {
                    Options.DecodeChildArgument(Convert.ToBase64String(nulArgument));
                }
                catch (ArgumentException error)
                {
                    nulRejected = error.Message.Contains("cannot contain NUL");
                }

                if (!nulRejected)
                {
                    throw new InvalidOperationException("decoded NUL child argument was not rejected");
                }

                string executable = "C:\\x.exe";
                int largestArgumentLength = 32766 - executable.Length - 1;
                List<string> largestArguments = new List<string>();
                largestArguments.Add(new string('a', largestArgumentLength));
                string largest = BuildCommandLine(executable, largestArguments);
                if (largest.Length + 1 != 32767)
                {
                    throw new InvalidOperationException("maximum command line did not include exactly one terminating NUL");
                }

                bool oversizedRejected = false;
                try
                {
                    List<string> oversizedArguments = new List<string>();
                    oversizedArguments.Add(new string('a', largestArgumentLength + 1));
                    BuildCommandLine(executable, oversizedArguments);
                }
                catch (ArgumentException error)
                {
                    oversizedRejected = error.Message.Contains("terminating NUL");
                }

                if (!oversizedRejected)
                {
                    throw new InvalidOperationException("command line with 32768 code units including NUL was not rejected");
                }

                Console.WriteLine("windows process audit helper internal unit tests passed");
                return 0;
            }
            catch (Exception error)
            {
                Console.Error.WriteLine("windows process audit helper internal unit test failure: " + error);
                return ObserverFailureExitCode;
            }
        }
#endif

        internal int Run()
        {
            if (Environment.Is64BitOperatingSystem && !Environment.Is64BitProcess)
            {
                throw new InvalidOperationException("64-bit helper process is required for the DEBUG_EVENT layout");
            }

            if (Marshal.SizeOf(typeof(DebugEvent)) != 176)
            {
                throw new InvalidOperationException("unexpected 64-bit DEBUG_EVENT size: " + Marshal.SizeOf(typeof(DebugEvent)));
            }

            Thread.BeginThreadAffinity();
            try
            {
                return RunOnCreatorThread();
            }
            finally
            {
                Thread.EndThreadAffinity();
            }
        }

        private int RunOnCreatorThread()
        {
            IntPtr stdoutRead = IntPtr.Zero;
            IntPtr stdoutWrite = IntPtr.Zero;
            IntPtr stderrRead = IntPtr.Zero;
            IntPtr stderrWrite = IntPtr.Zero;
            IntPtr nullInput = new IntPtr(-1);
            IntPtr environment = IntPtr.Zero;
            IntPtr attributeList = IntPtr.Zero;
            IntPtr inheritedHandleList = IntPtr.Zero;
            bool observerSucceeded = false;
            try
            {
                job = CreateCleanupJob();
                CreateOutputPipe(out stdoutRead, out stdoutWrite);
                CreateOutputPipe(out stderrRead, out stderrWrite);
                nullInput = CreateNullInput();
                environment = BuildEnvironmentBlock(options);

                StartupInfoEx startup = CreateExtendedStartupInfo(
                    nullInput,
                    stdoutWrite,
                    stderrWrite,
                    out attributeList,
                    out inheritedHandleList);
                ProcessInformation process;
                StringBuilder commandLine = new StringBuilder(BuildCommandLine(options.Executable, options.ChildArguments));
                uint flags = DEBUG_PROCESS | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT;
                bool created = Native.CreateProcess(
                    options.Executable,
                    commandLine,
                    IntPtr.Zero,
                    IntPtr.Zero,
                    true,
                    flags,
                    environment,
                    options.WorkingDirectory,
                    ref startup,
                    out process);
                int createError = Marshal.GetLastWin32Error();
                DeleteAttributeList(ref attributeList, ref inheritedHandleList);
                if (!created)
                {
                    throw new Win32Exception(createError, "CreateProcessW failed");
                }

                rootProcess = process.Process;
                rootThread = process.Thread;
                rootProcessId = process.ProcessId;
                if (!Native.DebugSetProcessKillOnExit(true))
                {
                    throw LastError("DebugSetProcessKillOnExit");
                }

#if PROCESS_AUDIT_TESTING
                if (options.TestFailBeforeJobAssignment)
                {
                    DebugEvent initialEvent;
                    if (!Native.WaitForDebugEvent(out initialEvent, 5000))
                    {
                        throw LastError("WaitForDebugEvent(test pre-job create)");
                    }

                    bool expectedRootCreate =
                        initialEvent.Code == CREATE_PROCESS_DEBUG_EVENT &&
                        initialEvent.ProcessId == rootProcessId;
                    ProcessAndContinueEvent(ref initialEvent, true);
                    if (!expectedRootCreate)
                    {
                        throw new InvalidOperationException(
                            "test pre-job injection received unexpected debug event code=" + initialEvent.Code +
                            " pid=" + initialEvent.ProcessId);
                    }

                    throw new InvalidOperationException("injected pre-job-assignment failure");
                }
#endif

                if (!Native.AssignProcessToJobObject(job, rootProcess))
                {
                    throw LastError("AssignProcessToJobObject");
                }

                rootAssignedToJob = true;

                CloseRequired(ref stdoutWrite, "CloseHandle(stdout child write)");
                CloseRequired(ref stderrWrite, "CloseHandle(stderr child write)");
                CloseRequired(ref nullInput, "CloseHandle(NUL input)");
                stdoutPump = new PipePump(stdoutRead, Console.OpenStandardOutput(), "stdout");
                stdoutRead = IntPtr.Zero;
                stderrPump = new PipePump(stderrRead, Console.OpenStandardError(), "stderr");
                stderrRead = IntPtr.Zero;
                stdoutPump.Start();
                stderrPump.Start();
                stopwatch.Start();
                uint resumeResult = Native.ResumeThread(rootThread);
                if (resumeResult == uint.MaxValue)
                {
                    throw LastError("ResumeThread");
                }

                CloseRequired(ref rootThread, "CloseHandle(root thread)");
                ObserveUntilComplete();
                JoinPumps();
                evidence.ChildExitCode = rootExitCode;
                evidence.ActiveProcessIdsAtFinish = SortedActiveIds();
                if (evidence.ActiveProcessIdsAtFinish.Count != 0)
                {
                    throw new InvalidOperationException("debug active set was not empty after root exit");
                }

                observerSucceeded = true;
                return unchecked((int)rootExitCode);
            }
            catch
            {
                CleanupFailedObservation();
                evidence.ActiveProcessIdsAtFinish = SortedActiveIds();
                throw;
            }
            finally
            {
                if (environment != IntPtr.Zero)
                {
                    Marshal.FreeHGlobal(environment);
                }

                DeleteAttributeList(ref attributeList, ref inheritedHandleList);

                CloseBestEffort(ref stdoutWrite);
                CloseBestEffort(ref stderrWrite);
                CloseBestEffort(ref stdoutRead);
                CloseBestEffort(ref stderrRead);
                if (nullInput != new IntPtr(-1))
                {
                    CloseBestEffort(ref nullInput);
                }

                CloseBestEffort(ref rootThread);
                CloseBestEffort(ref rootProcess);
                if (!observerSucceeded && job != IntPtr.Zero)
                {
                    Native.TerminateJobObject(job, ObserverFailureExitCode);
                }

                CloseBestEffort(ref job);
            }
        }

        private void ObserveUntilComplete()
        {
            while (!rootExitSeen || active.Count != 0)
            {
                ThrowIfObserverUnhealthy();
                DebugEvent debugEvent;
                if (!Native.WaitForDebugEvent(out debugEvent, 100))
                {
                    int error = Marshal.GetLastWin32Error();
                    if ((uint)error == ErrorSemTimeout)
                    {
                        continue;
                    }

                    throw new Win32Exception(error, "WaitForDebugEvent failed");
                }

                ProcessAndContinueEvent(ref debugEvent, true);
            }
        }

        private void ProcessAndContinueEvent(ref DebugEvent debugEvent, bool enforceForbidden)
        {
            uint continueStatus = debugEvent.Code == EXCEPTION_DEBUG_EVENT ? DbgExceptionNotHandled : DbgContinue;
            Exception handlerError = null;
            try
            {
                continueStatus = HandleEvent(ref debugEvent, enforceForbidden);
            }
            catch (Exception error)
            {
                handlerError = error;
            }

            if (!Native.ContinueDebugEvent(debugEvent.ProcessId, debugEvent.ThreadId, continueStatus))
            {
                Win32Exception continueError = LastError("ContinueDebugEvent");
                if (handlerError != null)
                {
                    throw new AggregateException(handlerError.Message + "; ContinueDebugEvent also failed", handlerError, continueError);
                }

                throw continueError;
            }

            if (handlerError != null)
            {
                throw new InvalidOperationException(handlerError.Message, handlerError);
            }
        }

        private void ThrowIfObserverUnhealthy()
        {
            if (stopwatch.ElapsedMilliseconds > options.TimeoutMs)
            {
                throw new TimeoutException("process-audit timeout after " + options.TimeoutMs + "ms");
            }

            Exception stdoutError = stdoutPump == null ? null : stdoutPump.Error;
            Exception stderrError = stderrPump == null ? null : stderrPump.Error;
            if (stdoutError != null)
            {
                throw new IOException("stdout drain failed", stdoutError);
            }

            if (stderrError != null)
            {
                throw new IOException("stderr drain failed", stderrError);
            }
        }

        private uint HandleEvent(ref DebugEvent debugEvent, bool enforceForbidden)
        {
            switch (ClassifyDebugEvent(ref debugEvent))
            {
                case DebugEventKind.CreateProcess:
                    HandleCreate(ref debugEvent, enforceForbidden);
                    return DbgContinue;
                case DebugEventKind.ExitProcess:
                    HandleExit(ref debugEvent);
                    return DbgContinue;
                case DebugEventKind.LoadDll:
                    CloseDebugFile(debugEvent.Data.LoadDll.File, "LOAD_DLL_DEBUG_EVENT hFile");
                    return DbgContinue;
                case DebugEventKind.Exception:
                    if (debugEvent.Data.ExceptionCode == ExceptionBreakpoint && initialBreakpoints.Add(debugEvent.ProcessId))
                    {
                        return DbgContinue;
                    }

                    return DbgExceptionNotHandled;
                case DebugEventKind.CreateThread:
                case DebugEventKind.ExitThread:
                case DebugEventKind.UnloadDll:
                case DebugEventKind.OutputDebugString:
                    return DbgContinue;
                default:
                    throw new InvalidOperationException("unhandled classified debug event kind: " + debugEvent.Code);
            }
        }

        private static DebugEventKind ClassifyDebugEvent(ref DebugEvent debugEvent)
        {
            switch (debugEvent.Code)
            {
                case EXCEPTION_DEBUG_EVENT:
                    return DebugEventKind.Exception;
                case CREATE_THREAD_DEBUG_EVENT:
                    return DebugEventKind.CreateThread;
                case CREATE_PROCESS_DEBUG_EVENT:
                    return DebugEventKind.CreateProcess;
                case EXIT_THREAD_DEBUG_EVENT:
                    return DebugEventKind.ExitThread;
                case EXIT_PROCESS_DEBUG_EVENT:
                    return DebugEventKind.ExitProcess;
                case LOAD_DLL_DEBUG_EVENT:
                    return DebugEventKind.LoadDll;
                case UNLOAD_DLL_DEBUG_EVENT:
                    return DebugEventKind.UnloadDll;
                case OUTPUT_DEBUG_STRING_EVENT:
                    return DebugEventKind.OutputDebugString;
                case RIP_EVENT:
                    throw new InvalidOperationException(
                        "RIP_EVENT reported error=" + debugEvent.Data.Rip.Error +
                        " type=" + debugEvent.Data.Rip.Type);
                default:
                    throw new InvalidOperationException("unknown debug event code: " + debugEvent.Code);
            }
        }

        private void HandleCreate(ref DebugEvent debugEvent, bool enforceForbidden)
        {
            IntPtr imageFile = debugEvent.Data.CreateProcess.File;
            string path = null;
            Exception resolutionError = null;
            try
            {
                try
                {
                    path = ResolveImagePath(imageFile, debugEvent.Data.CreateProcess.Process);
                }
                catch (Exception error)
                {
                    resolutionError = error;
                }
            }
            finally
            {
                CloseDebugFile(imageFile, "CREATE_PROCESS_DEBUG_EVENT hFile");
            }

            if (resolutionError != null)
            {
                throw new InvalidOperationException("failed to resolve image for pid " + debugEvent.ProcessId, resolutionError);
            }

            if (string.IsNullOrEmpty(path))
            {
                throw new InvalidOperationException("resolved empty image for pid " + debugEvent.ProcessId);
            }

            if (!active.Add(debugEvent.ProcessId))
            {
                throw new InvalidOperationException("duplicate CREATE_PROCESS_DEBUG_EVENT for pid " + debugEvent.ProcessId);
            }

            evidence.ProcessStarts.Add(new ProcessStartRecord(debugEvent.ProcessId, path));
            string image = Path.GetFileName(path);
            if (enforceForbidden && options.ForbiddenImages.Contains(image))
            {
                throw new InvalidOperationException("forbidden process image observed: " + image + " pid=" + debugEvent.ProcessId);
            }
        }

        private void HandleExit(ref DebugEvent debugEvent)
        {
            if (!active.Remove(debugEvent.ProcessId))
            {
                throw new InvalidOperationException("EXIT_PROCESS_DEBUG_EVENT without active pid " + debugEvent.ProcessId);
            }

            initialBreakpoints.Remove(debugEvent.ProcessId);

            uint exitCode = debugEvent.Data.ExitProcess.ExitCode;
            evidence.ProcessExits.Add(new ProcessExitRecord(debugEvent.ProcessId, exitCode));
            if (debugEvent.ProcessId == rootProcessId)
            {
                rootExitSeen = true;
                rootExitCode = exitCode;
            }
        }

        private void CleanupFailedObservation()
        {
            if (rootProcess != IntPtr.Zero && !rootAssignedToJob)
            {
                Native.TerminateProcess(rootProcess, ObserverFailureExitCode);
            }
            else if (job != IntPtr.Zero)
            {
                Native.TerminateJobObject(job, ObserverFailureExitCode);
            }

            Stopwatch cleanup = Stopwatch.StartNew();
            while (active.Count != 0 && cleanup.ElapsedMilliseconds < 5000)
            {
                DebugEvent debugEvent;
                if (!Native.WaitForDebugEvent(out debugEvent, 100))
                {
                    int error = Marshal.GetLastWin32Error();
                    if ((uint)error == ErrorSemTimeout)
                    {
                        continue;
                    }

                    break;
                }

                try
                {
                    ProcessAndContinueEvent(ref debugEvent, false);
                }
                catch
                {
                    continue;
                }
            }

            if (stdoutPump != null)
            {
                stdoutPump.Join(5000);
            }

            if (stderrPump != null)
            {
                stderrPump.Join(5000);
            }
        }

        private void JoinPumps()
        {
            if (!stdoutPump.Join(5000))
            {
                throw new TimeoutException("stdout drain did not finish after process exit");
            }

            if (!stderrPump.Join(5000))
            {
                throw new TimeoutException("stderr drain did not finish after process exit");
            }

            ThrowIfObserverUnhealthy();
        }

        private List<uint> SortedActiveIds()
        {
            List<uint> ids = new List<uint>(active);
            ids.Sort();
            return ids;
        }

        private static string ResolveImagePath(IntPtr imageFile, IntPtr process)
        {
            if (imageFile != IntPtr.Zero)
            {
                StringBuilder path = new StringBuilder(512);
                for (int attempt = 0; attempt < 3; attempt++)
                {
                    uint length = Native.GetFinalPathNameByHandle(imageFile, path, (uint)path.Capacity, 0);
                    if (length == 0)
                    {
                        break;
                    }

                    if (length < path.Capacity)
                    {
                        return path.ToString();
                    }

                    if (length > 32767)
                    {
                        throw new PathTooLongException("image path exceeds the Windows extended-path limit");
                    }

                    path = new StringBuilder(checked((int)length + 1));
                }
            }

            if (process != IntPtr.Zero)
            {
                StringBuilder path = new StringBuilder(32768);
                uint capacity = (uint)path.Capacity;
                if (Native.QueryFullProcessImageName(process, 0, path, ref capacity))
                {
                    return path.ToString();
                }
            }

            throw LastError("GetFinalPathNameByHandleW/QueryFullProcessImageNameW");
        }

        private static string BuildCommandLine(string executable, IList<string> arguments)
        {
            StringBuilder commandLine = new StringBuilder();
            AppendQuotedArgument(commandLine, executable);
            for (int index = 0; index < arguments.Count; index++)
            {
                commandLine.Append(' ');
                AppendQuotedArgument(commandLine, arguments[index]);
            }

            if (commandLine.Length >= 32767)
            {
                throw new ArgumentException("child command line including its terminating NUL exceeds 32767 UTF-16 code units");
            }

            return commandLine.ToString();
        }

        private static void AppendQuotedArgument(StringBuilder output, string argument)
        {
            bool needsQuotes = argument.Length == 0 || argument.IndexOfAny(new[] { ' ', '\t', '\n', '\v', '"' }) >= 0;
            if (!needsQuotes)
            {
                output.Append(argument);
                return;
            }

            output.Append('"');
            int backslashes = 0;
            for (int index = 0; index < argument.Length; index++)
            {
                char current = argument[index];
                if (current == '\\')
                {
                    backslashes++;
                    continue;
                }

                if (current == '"')
                {
                    output.Append('\\', checked(backslashes * 2 + 1));
                    output.Append('"');
                    backslashes = 0;
                    continue;
                }

                output.Append('\\', backslashes);
                backslashes = 0;
                output.Append(current);
            }

            output.Append('\\', checked(backslashes * 2));
            output.Append('"');
        }

        private static IntPtr BuildEnvironmentBlock(Options options)
        {
            SortedDictionary<string, string> variables = new SortedDictionary<string, string>(StringComparer.OrdinalIgnoreCase);
            if (options.EnvironmentMode == "inherit")
            {
                foreach (DictionaryEntry entry in Environment.GetEnvironmentVariables())
                {
                    variables[(string)entry.Key] = (string)entry.Value;
                }
            }

            foreach (KeyValuePair<string, string> entry in options.EnvironmentOverrides)
            {
                variables[entry.Key] = entry.Value;
            }

            StringBuilder block = new StringBuilder();
            foreach (KeyValuePair<string, string> entry in variables)
            {
                block.Append(entry.Key);
                block.Append('=');
                block.Append(entry.Value);
                block.Append('\0');
            }

            block.Append('\0');
            if (variables.Count == 0)
            {
                block.Append('\0');
            }

            byte[] bytes = Encoding.Unicode.GetBytes(block.ToString());
            IntPtr native = Marshal.AllocHGlobal(bytes.Length);
            Marshal.Copy(bytes, 0, native, bytes.Length);
            return native;
        }

        private static IntPtr CreateCleanupJob()
        {
            IntPtr handle = Native.CreateJobObject(IntPtr.Zero, null);
            if (handle == IntPtr.Zero)
            {
                throw LastError("CreateJobObjectW");
            }

            ExtendedLimitInformation limits = new ExtendedLimitInformation();
            limits.BasicLimitInformation.LimitFlags = JobObjectLimitKillOnJobClose;
            int size = Marshal.SizeOf(typeof(ExtendedLimitInformation));
            IntPtr native = Marshal.AllocHGlobal(size);
            try
            {
                Marshal.StructureToPtr(limits, native, false);
                if (!Native.SetInformationJobObject(handle, JobObjectExtendedLimitInformation, native, (uint)size))
                {
                    int error = Marshal.GetLastWin32Error();
                    Native.CloseHandle(handle);
                    throw new Win32Exception(error, "SetInformationJobObject failed");
                }
            }
            finally
            {
                Marshal.FreeHGlobal(native);
            }

            return handle;
        }

        private static void CreateOutputPipe(out IntPtr read, out IntPtr write)
        {
            SecurityAttributes attributes = new SecurityAttributes();
            attributes.Length = Marshal.SizeOf(typeof(SecurityAttributes));
            attributes.InheritHandle = 1;
            if (!Native.CreatePipe(out read, out write, ref attributes, 0))
            {
                throw LastError("CreatePipe");
            }

            if (!Native.SetHandleInformation(read, HandleFlagInherit, 0))
            {
                int error = Marshal.GetLastWin32Error();
                Native.CloseHandle(read);
                Native.CloseHandle(write);
                read = IntPtr.Zero;
                write = IntPtr.Zero;
                throw new Win32Exception(error, "SetHandleInformation failed");
            }
        }

        private static IntPtr CreateNullInput()
        {
            SecurityAttributes attributes = new SecurityAttributes();
            attributes.Length = Marshal.SizeOf(typeof(SecurityAttributes));
            attributes.InheritHandle = 1;
            IntPtr handle = Native.CreateFile(
                "NUL",
                GenericRead,
                FileShareRead | FileShareWrite,
                ref attributes,
                OpenExisting,
                FileAttributeNormal,
                IntPtr.Zero);
            if (handle == new IntPtr(-1))
            {
                throw LastError("CreateFileW(NUL)");
            }

            return handle;
        }

        private static StartupInfoEx CreateExtendedStartupInfo(
            IntPtr standardInput,
            IntPtr standardOutput,
            IntPtr standardError,
            out IntPtr attributeList,
            out IntPtr inheritedHandleList)
        {
            attributeList = IntPtr.Zero;
            inheritedHandleList = IntPtr.Zero;
            UIntPtr attributeListSize = UIntPtr.Zero;
            bool sizeProbe = Native.InitializeProcThreadAttributeList(IntPtr.Zero, 1, 0, ref attributeListSize);
            int sizeProbeError = Marshal.GetLastWin32Error();
            if (sizeProbe || (uint)sizeProbeError != ErrorInsufficientBuffer || attributeListSize.Equals(UIntPtr.Zero))
            {
                throw new Win32Exception(sizeProbeError, "InitializeProcThreadAttributeList size probe failed");
            }

            ulong nativeSize = attributeListSize.ToUInt64();
            if (nativeSize > int.MaxValue)
            {
                throw new InvalidOperationException("process attribute list is unexpectedly large: " + nativeSize);
            }

            attributeList = Marshal.AllocHGlobal((int)nativeSize);
            if (!Native.InitializeProcThreadAttributeList(attributeList, 1, 0, ref attributeListSize))
            {
                int error = Marshal.GetLastWin32Error();
                Marshal.FreeHGlobal(attributeList);
                attributeList = IntPtr.Zero;
                throw new Win32Exception(error, "InitializeProcThreadAttributeList failed");
            }

            int handleBytes = checked(IntPtr.Size * 3);
            inheritedHandleList = Marshal.AllocHGlobal(handleBytes);
            Marshal.WriteIntPtr(inheritedHandleList, 0, standardInput);
            Marshal.WriteIntPtr(inheritedHandleList, IntPtr.Size, standardOutput);
            Marshal.WriteIntPtr(inheritedHandleList, IntPtr.Size * 2, standardError);
            if (!Native.UpdateProcThreadAttribute(
                attributeList,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                inheritedHandleList,
                new UIntPtr((uint)handleBytes),
                IntPtr.Zero,
                IntPtr.Zero))
            {
                int error = Marshal.GetLastWin32Error();
                DeleteAttributeList(ref attributeList, ref inheritedHandleList);
                throw new Win32Exception(error, "UpdateProcThreadAttribute(PROC_THREAD_ATTRIBUTE_HANDLE_LIST) failed");
            }

            StartupInfoEx startup = new StartupInfoEx();
            startup.StartupInfo.Size = (uint)Marshal.SizeOf(typeof(StartupInfoEx));
            startup.StartupInfo.Flags = StartfUseStdHandles;
            startup.StartupInfo.StandardInput = standardInput;
            startup.StartupInfo.StandardOutput = standardOutput;
            startup.StartupInfo.StandardError = standardError;
            startup.AttributeList = attributeList;
            return startup;
        }

        private static void DeleteAttributeList(ref IntPtr attributeList, ref IntPtr inheritedHandleList)
        {
            if (attributeList != IntPtr.Zero)
            {
                Native.DeleteProcThreadAttributeList(attributeList);
                Marshal.FreeHGlobal(attributeList);
                attributeList = IntPtr.Zero;
            }

            if (inheritedHandleList != IntPtr.Zero)
            {
                Marshal.FreeHGlobal(inheritedHandleList);
                inheritedHandleList = IntPtr.Zero;
            }
        }

        private static void CloseDebugFile(IntPtr handle, string label)
        {
            if (handle != IntPtr.Zero && !Native.CloseHandle(handle))
            {
                throw LastError(label);
            }
        }

        private static void CloseRequired(ref IntPtr handle, string label)
        {
            if (handle == IntPtr.Zero || handle == new IntPtr(-1))
            {
                handle = IntPtr.Zero;
                return;
            }

            IntPtr closing = handle;
            handle = IntPtr.Zero;
            if (!Native.CloseHandle(closing))
            {
                throw LastError(label);
            }
        }

        private static void CloseBestEffort(ref IntPtr handle)
        {
            if (handle != IntPtr.Zero && handle != new IntPtr(-1))
            {
                Native.CloseHandle(handle);
            }

            handle = IntPtr.Zero;
        }

        private static Win32Exception LastError(string operation)
        {
            return new Win32Exception(Marshal.GetLastWin32Error(), operation + " failed");
        }

        private sealed class PipePump
        {
            private readonly IntPtr sourceHandle;
            private readonly Stream destination;
            private readonly string label;
            private readonly Thread thread;
            private Exception error;

            internal PipePump(IntPtr sourceHandle, Stream destination, string label)
            {
                this.sourceHandle = sourceHandle;
                this.destination = destination;
                this.label = label;
                thread = new Thread(Pump);
                thread.IsBackground = true;
                thread.Name = "process-audit-" + label;
            }

            internal Exception Error
            {
                get { return error; }
            }

            internal void Start()
            {
                thread.Start();
            }

            internal bool Join(int timeoutMs)
            {
                return thread.Join(timeoutMs);
            }

            private void Pump()
            {
                try
                {
                    using (SafeFileHandle safe = new SafeFileHandle(sourceHandle, true))
                    using (FileStream input = new FileStream(safe, FileAccess.Read, 8192, false))
                    {
                        byte[] buffer = new byte[8192];
                        while (true)
                        {
                            int count = input.Read(buffer, 0, buffer.Length);
                            if (count == 0)
                            {
                                break;
                            }

                            destination.Write(buffer, 0, count);
                            destination.Flush();
                        }
                    }
                }
                catch (Exception pumpError)
                {
                    error = new IOException(label + " pipe pump failed", pumpError);
                }
            }
        }
    }

    private sealed class AuditEvidence
    {
        internal const int SchemaVersion = 1;
        internal string Status = "failed";
        internal readonly string StartedAtUtc = Timestamp();
        internal string FinishedAtUtc;
        internal readonly string Executable;
        internal readonly int ArgumentCount;
        internal readonly string WorkingDirectory;
        internal readonly int TimeoutMs;
        internal readonly string EnvironmentMode;
        internal readonly List<string> EnvironmentOverrideNames;
        internal uint? ChildExitCode;
        internal string ObserverError;
        internal readonly List<ProcessStartRecord> ProcessStarts = new List<ProcessStartRecord>();
        internal readonly List<ProcessExitRecord> ProcessExits = new List<ProcessExitRecord>();
        internal List<uint> ActiveProcessIdsAtFinish = new List<uint>();

        internal AuditEvidence(Options options)
        {
            Executable = options.Executable;
            ArgumentCount = options.ChildArguments.Count;
            WorkingDirectory = options.WorkingDirectory;
            TimeoutMs = options.TimeoutMs;
            EnvironmentMode = options.EnvironmentMode;
            EnvironmentOverrideNames = new List<string>(options.EnvironmentOverrides.Keys);
        }
    }

    private sealed class ProcessStartRecord
    {
        internal readonly uint ProcessId;
        internal readonly string Path;
        internal readonly string ObservedAtUtc;

        internal ProcessStartRecord(uint processId, string path)
        {
            ProcessId = processId;
            Path = path;
            ObservedAtUtc = Timestamp();
        }
    }

    private sealed class ProcessExitRecord
    {
        internal readonly uint ProcessId;
        internal readonly uint ExitCode;
        internal readonly string ObservedAtUtc;

        internal ProcessExitRecord(uint processId, uint exitCode)
        {
            ProcessId = processId;
            ExitCode = exitCode;
            ObservedAtUtc = Timestamp();
        }
    }

    private static class EvidenceWriter
    {
        internal static void Write(string path, AuditEvidence evidence)
        {
            string temporary = path + ".tmp-" + Guid.NewGuid().ToString("N");
            try
            {
                using (FileStream file = new FileStream(temporary, FileMode.CreateNew, FileAccess.Write, FileShare.None))
                using (StreamWriter output = new StreamWriter(file, new UTF8Encoding(false)))
                {
                    output.Write('{');
                    Property(output, "schema_version", AuditEvidence.SchemaVersion.ToString(CultureInfo.InvariantCulture), false, false);
                    Property(output, "status", evidence.Status, true, true);
                    Property(output, "started_at_utc", evidence.StartedAtUtc, true, true);
                    Property(output, "finished_at_utc", evidence.FinishedAtUtc, true, true);
                    Property(output, "executable", evidence.Executable, true, true);
                    Property(output, "argument_count", evidence.ArgumentCount.ToString(CultureInfo.InvariantCulture), false, true);
                    Property(output, "working_directory", evidence.WorkingDirectory, true, true);
                    Property(output, "timeout_ms", evidence.TimeoutMs.ToString(CultureInfo.InvariantCulture), false, true);
                    Property(output, "environment_mode", evidence.EnvironmentMode, true, true);
                    output.Write(",\"environment_override_names\":[");
                    WriteStringList(output, evidence.EnvironmentOverrideNames);
                    output.Write(']');
                    output.Write(",\"child_exit_code\":");
                    if (evidence.ChildExitCode.HasValue)
                    {
                        output.Write(evidence.ChildExitCode.Value.ToString(CultureInfo.InvariantCulture));
                    }
                    else
                    {
                        output.Write("null");
                    }

                    output.Write(",\"observer_error\":");
                    if (evidence.ObserverError == null)
                    {
                        output.Write("null");
                    }
                    else
                    {
                        WriteJsonString(output, evidence.ObserverError);
                    }

                    output.Write(",\"process_starts\":[");
                    for (int index = 0; index < evidence.ProcessStarts.Count; index++)
                    {
                        if (index != 0)
                        {
                            output.Write(',');
                        }

                        ProcessStartRecord record = evidence.ProcessStarts[index];
                        output.Write('{');
                        Property(output, "process_id", record.ProcessId.ToString(CultureInfo.InvariantCulture), false, false);
                        Property(output, "path", record.Path, true, true);
                        Property(output, "observed_at_utc", record.ObservedAtUtc, true, true);
                        output.Write('}');
                    }

                    output.Write(']');
                    output.Write(",\"process_exits\":[");
                    for (int index = 0; index < evidence.ProcessExits.Count; index++)
                    {
                        if (index != 0)
                        {
                            output.Write(',');
                        }

                        ProcessExitRecord record = evidence.ProcessExits[index];
                        output.Write('{');
                        Property(output, "process_id", record.ProcessId.ToString(CultureInfo.InvariantCulture), false, false);
                        Property(output, "exit_code", record.ExitCode.ToString(CultureInfo.InvariantCulture), false, true);
                        Property(output, "observed_at_utc", record.ObservedAtUtc, true, true);
                        output.Write('}');
                    }

                    output.Write(']');
                    output.Write(",\"active_process_ids_at_finish\":[");
                    for (int index = 0; index < evidence.ActiveProcessIdsAtFinish.Count; index++)
                    {
                        if (index != 0)
                        {
                            output.Write(',');
                        }

                        output.Write(evidence.ActiveProcessIdsAtFinish[index].ToString(CultureInfo.InvariantCulture));
                    }

                    output.Write("]}");
                }

                if (File.Exists(path))
                {
                    throw new IOException("refusing to overwrite existing evidence: " + path);
                }

                File.Move(temporary, path);
            }
            finally
            {
                if (File.Exists(temporary))
                {
                    File.Delete(temporary);
                }
            }
        }

        private static void Property(StreamWriter output, string name, string value, bool quote, bool comma)
        {
            if (comma)
            {
                output.Write(',');
            }

            WriteJsonString(output, name);
            output.Write(':');
            if (quote)
            {
                WriteJsonString(output, value);
            }
            else
            {
                output.Write(value);
            }
        }

        private static void WriteStringList(StreamWriter output, IList<string> values)
        {
            for (int index = 0; index < values.Count; index++)
            {
                if (index != 0)
                {
                    output.Write(',');
                }

                WriteJsonString(output, values[index]);
            }
        }

        private static void WriteJsonString(StreamWriter output, string value)
        {
            output.Write('"');
            for (int index = 0; index < value.Length; index++)
            {
                char current = value[index];
                switch (current)
                {
                    case '"':
                        output.Write("\\\"");
                        break;
                    case '\\':
                        output.Write("\\\\");
                        break;
                    case '\b':
                        output.Write("\\b");
                        break;
                    case '\f':
                        output.Write("\\f");
                        break;
                    case '\n':
                        output.Write("\\n");
                        break;
                    case '\r':
                        output.Write("\\r");
                        break;
                    case '\t':
                        output.Write("\\t");
                        break;
                    default:
                        if (current < 0x20)
                        {
                            output.Write("\\u");
                            output.Write(((int)current).ToString("x4", CultureInfo.InvariantCulture));
                        }
                        else
                        {
                            output.Write(current);
                        }

                        break;
                }
            }

            output.Write('"');
        }
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct SecurityAttributes
    {
        internal int Length;
        internal IntPtr SecurityDescriptor;
        internal int InheritHandle;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct StartupInfo
    {
        internal uint Size;
        internal IntPtr Reserved;
        internal IntPtr Desktop;
        internal IntPtr Title;
        internal uint X;
        internal uint Y;
        internal uint XSize;
        internal uint YSize;
        internal uint XCountChars;
        internal uint YCountChars;
        internal uint FillAttribute;
        internal uint Flags;
        internal ushort ShowWindow;
        internal ushort ReservedSize;
        internal IntPtr ReservedPointer;
        internal IntPtr StandardInput;
        internal IntPtr StandardOutput;
        internal IntPtr StandardError;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct StartupInfoEx
    {
        internal StartupInfo StartupInfo;
        internal IntPtr AttributeList;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct ProcessInformation
    {
        internal IntPtr Process;
        internal IntPtr Thread;
        internal uint ProcessId;
        internal uint ThreadId;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct BasicLimitInformation
    {
        internal long PerProcessUserTimeLimit;
        internal long PerJobUserTimeLimit;
        internal uint LimitFlags;
        internal UIntPtr MinimumWorkingSetSize;
        internal UIntPtr MaximumWorkingSetSize;
        internal uint ActiveProcessLimit;
        internal UIntPtr Affinity;
        internal uint PriorityClass;
        internal uint SchedulingClass;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct IoCounters
    {
        internal ulong ReadOperationCount;
        internal ulong WriteOperationCount;
        internal ulong OtherOperationCount;
        internal ulong ReadTransferCount;
        internal ulong WriteTransferCount;
        internal ulong OtherTransferCount;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct ExtendedLimitInformation
    {
        internal BasicLimitInformation BasicLimitInformation;
        internal IoCounters IoInfo;
        internal UIntPtr ProcessMemoryLimit;
        internal UIntPtr JobMemoryLimit;
        internal UIntPtr PeakProcessMemoryUsed;
        internal UIntPtr PeakJobMemoryUsed;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct CreateProcessDebugInfo
    {
        internal IntPtr File;
        internal IntPtr Process;
        internal IntPtr Thread;
        internal IntPtr BaseOfImage;
        internal uint DebugInfoFileOffset;
        internal uint DebugInfoSize;
        internal IntPtr ThreadLocalBase;
        internal IntPtr StartAddress;
        internal IntPtr ImageName;
        internal ushort Unicode;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct LoadDllDebugInfo
    {
        internal IntPtr File;
        internal IntPtr BaseOfDll;
        internal uint DebugInfoFileOffset;
        internal uint DebugInfoSize;
        internal IntPtr ImageName;
        internal ushort Unicode;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct ExitProcessDebugInfo
    {
        internal uint ExitCode;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct RipInfo
    {
        internal uint Error;
        internal uint Type;
    }

    [StructLayout(LayoutKind.Explicit, Size = 160)]
    private struct DebugEventData
    {
        [FieldOffset(0)]
        internal CreateProcessDebugInfo CreateProcess;

        [FieldOffset(0)]
        internal LoadDllDebugInfo LoadDll;

        [FieldOffset(0)]
        internal ExitProcessDebugInfo ExitProcess;

        [FieldOffset(0)]
        internal RipInfo Rip;

        [FieldOffset(0)]
        internal uint ExceptionCode;

        [FieldOffset(152)]
        internal uint ExceptionFirstChance;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct DebugEvent
    {
        internal uint Code;
        internal uint ProcessId;
        internal uint ThreadId;
        internal DebugEventData Data;
    }

    private static class Native
    {
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true, EntryPoint = "CreateProcessW")]
        [return: MarshalAs(UnmanagedType.Bool)]
        internal static extern bool CreateProcess(
            string applicationName,
            StringBuilder commandLine,
            IntPtr processAttributes,
            IntPtr threadAttributes,
            [MarshalAs(UnmanagedType.Bool)] bool inheritHandles,
            uint creationFlags,
            IntPtr environment,
            string currentDirectory,
            ref StartupInfoEx startupInfo,
            out ProcessInformation processInformation);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        internal static extern bool InitializeProcThreadAttributeList(
            IntPtr attributeList,
            int attributeCount,
            uint flags,
            ref UIntPtr size);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        internal static extern bool UpdateProcThreadAttribute(
            IntPtr attributeList,
            uint flags,
            UIntPtr attribute,
            IntPtr value,
            UIntPtr size,
            IntPtr previousValue,
            IntPtr returnSize);

        [DllImport("kernel32.dll")]
        internal static extern void DeleteProcThreadAttributeList(IntPtr attributeList);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        internal static extern bool WaitForDebugEvent(out DebugEvent debugEvent, uint milliseconds);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        internal static extern bool ContinueDebugEvent(uint processId, uint threadId, uint continueStatus);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        internal static extern bool DebugSetProcessKillOnExit([MarshalAs(UnmanagedType.Bool)] bool killOnExit);

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true, EntryPoint = "GetFinalPathNameByHandleW")]
        internal static extern uint GetFinalPathNameByHandle(IntPtr file, StringBuilder path, uint pathLength, uint flags);

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true, EntryPoint = "QueryFullProcessImageNameW")]
        [return: MarshalAs(UnmanagedType.Bool)]
        internal static extern bool QueryFullProcessImageName(IntPtr process, uint flags, StringBuilder path, ref uint pathLength);

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true, EntryPoint = "CreateJobObjectW")]
        internal static extern IntPtr CreateJobObject(IntPtr jobAttributes, string name);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        internal static extern bool SetInformationJobObject(IntPtr job, int informationClass, IntPtr information, uint informationLength);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        internal static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        internal static extern bool TerminateJobObject(IntPtr job, int exitCode);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        internal static extern bool TerminateProcess(IntPtr process, int exitCode);

        [DllImport("kernel32.dll", SetLastError = true)]
        internal static extern uint ResumeThread(IntPtr thread);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        internal static extern bool CreatePipe(out IntPtr readPipe, out IntPtr writePipe, ref SecurityAttributes pipeAttributes, uint size);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        internal static extern bool SetHandleInformation(IntPtr handle, uint mask, uint flags);

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true, EntryPoint = "CreateFileW")]
        internal static extern IntPtr CreateFile(
            string fileName,
            uint desiredAccess,
            uint shareMode,
            ref SecurityAttributes securityAttributes,
            uint creationDisposition,
            uint flagsAndAttributes,
            IntPtr templateFile);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        internal static extern bool CloseHandle(IntPtr handle);
    }
}
