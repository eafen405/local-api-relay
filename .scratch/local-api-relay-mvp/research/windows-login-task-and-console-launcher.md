# Windows Login Task and Console Launcher for the Local Relay MVP

## Research question

How should ticket 30 implement (PKG-005) a per-user Windows scheduled task,
triggered at Windows user logon, that directly holds a long-running
`wsl.exe` invocation running `local-api-relay serve`; (PKG-006) a bounded
abnormal-exit restart policy that never restarts infinitely and never runs
before Windows logon; (PKG-008) a desktop console launcher that checks a
dedicated local ready endpoint and opens the Windows default browser to the
management page or shows status and actionable diagnostics; and (SEC-005)
ensures no bootstrap credentials or secrets live in the task, process
environment, normal logs, or the launcher? Every claim below traces to a
Microsoft Learn primary source, to the local `schtasks.exe` / `wsl.exe`
help output, or to behavior observed live in this environment.

## Source basis and freshness limit

All documentation was retrieved on **2026-08-11** from Microsoft Learn
(`learn.microsoft.com`). The local primary sources were captured on the same
date on a Windows 11 Home (Chinese edition), `NT 10.0.26200.0` (build
26200), host `LAPTOP-45RC44AF`; the interactive user is `user` (SID
`S-1-5-21-4053945493-3504556832-829198439-1001`), a standard-token account
(local Administrators group present but UAC deny-only), running in
interactive session 12 at Medium integrity. The WSL side is the default
distro `Ubuntu` (marked `*` in `wsl.exe --list --verbose`), WSL version 2,
currently running. `schtasks.exe` and `wsl.exe` output on this host is
localized to Chinese (GBK / UTF-16); quoted strings below are translations
of that captured output, and English field names are taken from the
corresponding Microsoft Learn pages.

Claims are marked throughout as **(live)** — reproduced with the exact
commands in this environment — **(doc)** — backed only by a cited Microsoft
Learn page — or **(gap)** — no authoritative source exists and the claim
must not be relied on.

## 1. Per-user ONLOGON task via `schtasks.exe`

### 1.1 `schtasks.exe` switch-based `/SC ONLOGON` creates an *any-user* logon trigger and was access-denied here

The Microsoft Learn reference defines `/SC ONLOGON` as "Specifies that the
task runs whenever a user (any user) logs on" [schtasks create, /sc
parameter](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/schtasks-create). The
local `schtasks.exe /Create /?` help lists the same schedule type without any
switch to bind the trigger to a specific user (the `-User` binding exists
only in the XML/PowerShell representations, see §1.2). The `/mo` table in
the Learn page adds "ONLOGON - Specifies that the task runs when the user
specified by the /ru parameter logs on", so the reference is internally
inconsistent; the *live* behavior is authoritative here:

- **(live)** `schtasks.exe /Create /SC ONLOGON /TN larr-research-test /TR "wsl.exe -d Ubuntu -- /bin/echo hello" /RL LIMITED /IT /F` → `错误: 拒绝访问。` ("ERROR: Access is denied").
- **(live)** The same command with explicit `/RU "laptop-45rc44af\user"` → access denied. `/SC ONSTART` → access denied. `/SC ONCE` with the same switches → `成功: 成功创建计划任务 ...` ("SUCCESS: created").
- **(live)** `Register-ScheduledTask` with `New-ScheduledTaskTrigger -AtLogOn` or `-AtStartup` (no `-User`) → `HRESULT 0x80070005` (`E_ACCESSDENIED`); with `-Once` → success. `New-ScheduledTaskTrigger -AtLogOn -User "LAPTOP-45RC44AF\user"` + an explicit `-LogonType Interactive` principal → **registered successfully**.

Conclusion: for a standard user, the switch-based `/SC ONLOGON` (any-user
trigger) is privileged and was denied in this environment; the **per-user**
logon trigger (logon of one specific user) is what a standard user can
register. The install script must therefore not use the switch route.

### 1.2 The per-user logon trigger shape, verified both ways

Both representations that bind the trigger to a user worked live:

- **(live) XML route:** `schtasks.exe /Create /TN larr-research-xml /XML <file> /F` where the file carries `<Triggers><LogonTrigger><UserId>LAPTOP-45RC44AF\user</UserId></LogonTrigger></Triggers>` and `<Principal><UserId>LAPTOP-45RC44AF\user</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal>` → created successfully. The file was passed to `schtasks.exe` as a UNC path `\\wsl.localhost\Ubuntu\tmp\...` (a Windows-side path is required; `schtasks.exe` accepts the `\\wsl.localhost` UNC), encoded UTF-16LE with BOM like `schtasks /Query /XML` emits.
- **(live) PowerShell route:** `Register-ScheduledTask` with `New-ScheduledTaskTrigger -AtLogOn -User "LAPTOP-45RC44AF\user"`, `New-ScheduledTaskPrincipal -UserId "LAPTOP-45RC44AF\user" -LogonType Interactive -RunLevel Limited` → registered successfully. The `-User` parameter of `New-ScheduledTaskTrigger` is documented as "Specifies the identifier of the user for a trigger that starts a task when a user logs on" [New-ScheduledTaskTrigger, -User](https://learn.microsoft.com/en-us/powershell/module/scheduledtasks/new-scheduledtasktrigger).
- **(live)** The `LogonTrigger` schema element carries the per-user binding: "If you want a task to be triggered when any member of a group logs on... do not assign a value to the LogonTrigger.UserId property" [LogonTrigger object, UserId property](https://learn.microsoft.com/en-us/windows/win32/taskschd/logontrigger).

A real-world example already on this machine confirms the same shape:
`\OpenClaw Gateway` (a per-user logon task) exports as
`<LogonType>InteractiveToken</LogonType>` with
`<Triggers><LogonTrigger><UserId>LAPTOP-45RC44AF\user</UserId></LogonTrigger></Triggers>`
**(live**, via `schtasks.exe /Query /TN "\OpenClaw Gateway" /XML`).

### 1.3 Real `/Query /TN <name> /V /FO LIST` field output

**(live)** For the XML-created per-user ONLOGON task `larr-research-xml`, the
verbose LIST query printed (Chinese locale; English names from the Learn
page's own verbose-output example [schtasks query, example](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/schtasks-query)):

| Field (this host, Chinese) | English name (Learn example) | Observed value |
| --- | --- | --- |
| 登录状态 | Logon Mode | `只使用交互方式` = Interactive only |
| 作为用户运行 | Run As User | `user` |
| 计划类型 | Scheduled Type / Schedule | `登陆时` = At logon |
| 要运行的任务 | Task To Run | `wsl.exe -d Ubuntu -- /bin/echo hello-from-lar-task` (the `/TR` verbatim) |
| 上次运行时间 | Last Run Time | (before first run) `1999/11/30 0:00:00` |
| 上次结果 | Last Result | `267011` = `0x41303` `SCHED_S_TASK_HAS_NOT_RUN` before the first run; `0` after a successful run; `1` after a run of `/bin/false` |
| 计划任务状态 | Scheduled Task State | 已启用 = Enabled |

The Learn reference documents that a task with the interactive-only property
shows "the Logon Mode field has a value of Interactive only" in a verbose
query [schtasks create, /it](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/schtasks-create).

### 1.4 "Run only when user is logged on" vs "Run whether user is logged on or not" and the password implication (SEC-005)

The logon type is carried by the principal's `<LogonType>` element, documented
as [LogonType (principalType) Element](https://learn.microsoft.com/en-us/windows/win32/taskschd/taskschedulerschema-logontype-principaltype-element):

- `InteractiveToken` — "User must already be logged on. The task will be run only in an existing interactive session." This is the "Run only when user is logged on" mode; **no password is stored in the task**.
- `S4U` — "no password is stored by the system and there is no access to the network or encrypted files" (background-capable, password-free).
- `Password` — "User must log on using a password" (the password is stored by Task Scheduler).

`schtasks.exe` exposes the same distinction as `/it` ("run the scheduled task only when the run as user is logged on"; note that `/it` is one-way — "You can't use a change command to remove the interactive-only property") and `/np` ("No password is stored. The task runs non-interactively as the given user. Only local resources are available") [schtasks create, /it and /np](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/schtasks-create).

**(live)** A task created with `InteractiveToken` contains **no credential
material at all**: the exported XML has only `<UserId>SID</UserId>` (the
SID form was written back on registration), `<LogonType>InteractiveToken</LogonType>`,
the `LogonTrigger`, and the `wsl.exe` action — no `<Password>`, no
`<Principal>` password attribute. This satisfies SEC-005 for the task side:
an `InteractiveToken` per-user logon task stores no bootstrap credential.

Note: `schtasks` prompts for a password when it considers one needed —
"Schtasks always prompts for a password unless you provide one, even when
you schedule a task on the local computer using the current user account"
[schtasks create, notes](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/schtasks-create). **(live)** A `schtasks.exe /Change /TR ...` on the InteractiveToken task printed `请输入 user 的密码:` ("Please enter the password for user:") and the change did not apply when no password was supplied — the installer should prefer `Set-ScheduledTask -InputObject` or the XML route over `/Change` when modifying a task.

## 2. Bounded restart-on-failure policy

### 2.1 `schtasks.exe` does NOT expose restart-on-failure

**(live)** The complete switch inventory of `schtasks.exe /Create /?`
(`/S /U /P /RU /RP /SC /MO /D /M /I /TN /TR /ST /RI /ET /DU /K /SD /ED /EC
/IT /NP /Z /XML /V1 /F /RL /HRESULT`) and of `/Change /?` (`/S /U /P /TN
/RU /RP /TR /ST /RI /ET /DU /K /SD /ED /IT /RL /ENABLE /DISABLE /Z /DELAY
/HRESULT`) contains **no restart-count or restart-interval switch**. The
`/RI` switch is the *repetition* interval for recurring schedules and is
explicitly "not applicable for schedule types: MINUTE, HOURLY, ONSTART,
ONLOGON, ONIDLE, ONEVENT" — it is unrelated to restart-on-failure. The
Learn `schtasks create` parameter table confirms the same switch set and
adds no restart switch [schtasks create, Parameters](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/schtasks-create). **Consequently `schtasks.exe` alone cannot set the restart policy; the PowerShell cmdlets or the XML import are required.**

### 2.2 The mechanism, with primary sources

The settings exist in the Task Scheduler engine as `ITaskSettings.RestartCount`
("the number of times that the Task Scheduler will attempt to restart the
task" [TaskSettings.RestartCount](https://learn.microsoft.com/en-us/windows/win32/taskschd/tasksettings-restartcount)) and
`ITaskSettings.RestartInterval` ("a value that specifies how long the Task
Scheduler will attempt to restart the task... If this property is set, the
RestartCount property must also be set... PT5M is 5 minutes... maximum 31
days, minimum 1 minute" [TaskSettings.RestartInterval](https://learn.microsoft.com/en-us/windows/win32/taskschd/tasksettings-restartinterval)).
In the task XML they are `<Settings><RestartOnFailure><Count>N</Count><Interval>PTnM</Interval></RestartOnFailure></Settings>`:
"Specifies that the Task Scheduler will attempt to restart the task if the
task fails for any reason", with `Count` an `unsignedByte` ≥ 1 and
`Interval` a duration in `PT1M..P31D`; "Both child elements must be set"
[RestartOnFailure (settingsType)](https://learn.microsoft.com/en-us/windows/win32/taskschd/taskschedulerschema-restartonfailure-settingstype-element),
[restartType Complex Type](https://learn.microsoft.com/en-us/windows/win32/taskschd/taskschedulerschema-restarttype-complextype).

The PowerShell route is documented: `New-ScheduledTaskSettingsSet
-RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 60)` — "Task
Scheduler attempts three restarts of the task at sixty minute intervals"
[New-ScheduledTaskSettingsSet, Example 3](https://learn.microsoft.com/en-us/powershell/module/scheduledtasks/new-scheduledtasksettingsset),
and `-RestartCount` / `-RestartInterval` are real parameters ("Specifies the
number of times that Task Scheduler attempts to restart the task"; "Specifies
the amount of time that Task Scheduler attempts to restart the task")
[New-ScheduledTaskSettingsSet, Parameters](https://learn.microsoft.com/en-us/powershell/module/scheduledtasks/new-scheduledtasksettingsset).
The settings object is applied at registration via `Register-ScheduledTask
-Settings` [Register-ScheduledTask](https://learn.microsoft.com/en-us/powershell/module/scheduledtasks/register-scheduledtask)
or afterwards via `Set-ScheduledTask -Settings` / `Set-ScheduledTask
-InputObject` [Set-ScheduledTask](https://learn.microsoft.com/en-us/powershell/module/scheduledtasks/set-scheduledtask).

### 2.3 Live experiments — boundedness holds; re-launch cadence not observed

Three throwaway tasks were created (each deleted immediately after):

1. **(live)** Per-user ONLOGON task, action `wsl.exe -d Ubuntu -- /bin/echo hello-from-lar-task`, `RestartOnFailure Count=3 / Interval=PT1M`: `schtasks.exe /Run` → Last Result `0`. Changed action to `wsl.exe -d Ubuntu -- /bin/false` via `Set-ScheduledTask -InputObject` (settings preserved, verified by `/XML` round-trip); `/Run` → Last Result `1`; **Last Run Time and Last Result remained unchanged for the following 3.5 minutes** (1-minute restart interval would have required re-launches by then).
2. **(live)** `-Once` time trigger + Interactive principal + `-RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)` created via PowerShell; trigger fired at the scheduled time, action failed with exit 1 (Last Result `1`); **no re-launch in 4 minutes** (polled `schtasks /Query /V /FO LIST` every 20 s).
3. **(live)** The same shape created via `schtasks.exe /Create /XML` (the recommended installer route); **no re-launch in 4.5 minutes**.

So in this environment the documented "will attempt to restart" mechanism
did not visibly re-launch a failing interactive task within 4× the restart
interval, for any of the three creation/trigger paths. What *was* verified
is the property PKG-006 actually requires on the failure side: **the task
stayed failed and never re-ran — there is no infinite-restart loop**
(restarts are inherently bounded by the finite `Count`; the schema type is
`unsignedByte` and the PowerShell parameter is a plain `Int32`
[Count element](https://learn.microsoft.com/en-us/windows/win32/taskschd/taskschedulerschema-count-restarttype-element),
[New-ScheduledTaskSettingsSet](https://learn.microsoft.com/en-us/powershell/module/scheduledtasks/new-scheduledtasksettingsset)).

Two caveats for the implementer:

- **(gap)** No Microsoft Learn page states the exact conditions under which
  restarts fire (for example whether `InteractiveToken` principals get
  restarts at all, or whether restarts require a stored-password/S4U
  principal). The C++/scripting property pages only say the scheduler
  "will attempt" [TaskSettings.RestartCount](https://learn.microsoft.com/en-us/windows/win32/taskschd/tasksettings-restartcount).
  The S4U and `Password` logon types could not be exercised here: S4U
  registration from the WSL interop token was access-denied (`0x80070005`,
  **(live)**), and the `Password` type requires credentials we do not have.
- **(doc)** The "after the bound is exceeded it stays failed" end-state is
  the only reading consistent with a *counted* restart ("attempts three
  restarts... at sixty minute intervals" [New-ScheduledTaskSettingsSet](https://learn.microsoft.com/en-us/powershell/module/scheduledtasks/new-scheduledtasksettingsset))
  and is what the live runs showed; there is no documented "retry forever"
  mode.

The recommended installer behavior is therefore: **always register the task
with `RestartOnFailure` (`Count 3, Interval PT1M`), and design the recorded
acceptance check around the verifiable properties** — the task XML contains
the bounded restart settings, and after repeated failures the task remains
failed without looping (both verified live) — rather than around observing a
specific number of re-launches.

## 3. `wsl.exe` long-running invocation

### 3.1 CLI shape

The local `wsl.exe --help` (captured **(live)**) documents:
`wsl.exe [Argument] [Options...] [CommandLine]`, with `--exec, -e <CommandLine>`
("execute the specified command without using the default Linux shell"),
`--` ("pass the remaining command line through as-is"), `--cd <Directory>`,
`--distribution, -d <DistroName>`, and `--user, -u <UserName>`. The Learn
reference documents `wsl --distribution <Distribution Name> --user <User
Name>` — "To run a specific Linux distribution with a specific user...
If the user doesn't exist in the WSL distribution, you will receive an
error" [Basic commands for WSL](https://learn.microsoft.com/en-us/windows/wsl/basic-commands).

Round-trips from Windows, all **(live)** via `cmd.exe /c`:

- `wsl.exe -d Ubuntu -- echo ok` → prints `ok`, exit `0` (the exact shape
  ticket 30's task will run).
- `wsl.exe -d Ubuntu -e /bin/echo via-e-flag` and `-u root -e id -un` →
  works; `-u` selects the WSL user.
- Multi-argument passing: `-e /bin/echo hello world` and
  `-- /bin/echo hello world` both print `hello world` — arguments after the
  command are forwarded.

### 3.2 It stays attached until the child exits, and propagates the exit code

- **(live)** `cmd.exe /c wsl.exe -d Ubuntu -- sleep 2` took 2.13 s wall time
  — `wsl.exe` does not return while the launched process runs. The Learn
  networking page describes the attached model: "host command wsl.exe
  launches the target instance and executes Linux command... The STDOUT
  text content is then relayed back to wsl.exe. Finally, wsl.exe displays
  that output to the command line" [Accessing network applications with
  WSL, Identify IP address](https://learn.microsoft.com/en-us/windows/wsl/networking).
  This is exactly the property PKG-005 relies on: the scheduled task's
  `wsl.exe` process stays alive for as long as `local-api-relay serve`
  runs, so the WSL2 VM is not shut down while the task is alive.
- **(live)** Exit-code propagation: `wsl.exe -d Ubuntu -- sh -c 'exit 7'` →
  exit `7`; `exit 0` → `0`; `exit 5` → `5`; `exit 300` → `44` (300 mod
  256). So codes 0–255 are propagated exactly; values above 255 wrap at the
  byte level. **(gap)** No Microsoft Learn page documents the `wsl.exe` exit
  code (the "Basic commands" page does not mention it), so the ≤255 exact
  propagation is live-verified and the >255 wrapping must be treated as
  undocumented: the relay's serve process should exit with a code in 0–255
  if the task's Last Result must reflect it precisely.

### 3.3 Default-distro selection and why to pin `-d` and `-u`

- **(live)** `wsl.exe --list --verbose` prints the default distro marker:
  `* Ubuntu   Running   2`, and `wsl.exe --status` reports the default
  distribution and default version. The `*` marker and `--set-default
  <Distribution Name>` ("set the default Linux distribution that WSL
  commands will use to run") are documented [Basic commands for WSL](https://learn.microsoft.com/en-us/windows/wsl/basic-commands).
- **(doc→live)** A bare `wsl.exe` command picks the default distro, which
  the user can change at any time; the task must be deterministic, so the
  `/TR` must pin `-d <distro>` (and `-u <user>` so it runs as the intended
  WSL user regardless of the distro's default user). The scheduled task was
  verified live running a pinned-distro invocation through Task Scheduler
  (`schtasks /Run` → Last Result 0/1, §1.3/§2.3).

### 3.4 What happens at Windows logoff

- **(gap)** No Microsoft Learn page states what happens to a
  `wsl.exe`-launched child when the Windows user logs off (the FAQ and
  networking pages do not cover it). Do not rely on any particular
  kill-or-keep behavior. The design does not need to: an `InteractiveToken`
  task "will be run only in an existing interactive session" [LogonType
  element](https://learn.microsoft.com/en-us/windows/win32/taskschd/taskschedulerschema-logontype-principaltype-element),
  so at logoff the task cannot keep running anyway, and PKG-006 only
  requires the task to *start* at logon.
- **(doc)** The closest documented lifetime statement is from the WSL FAQ:
  "If you have no open file handles to Windows processes, the WSL VM will
  automatically be shut down. This means if you are using it as a web
  server, SSH into it to run your server and then exit, the VM could shut
  down..." [WSL FAQ, production scenarios](https://learn.microsoft.com/en-us/windows/wsl/faq).
  This is precisely why PKG-005 holds the long-running `wsl.exe` in the
  task: while that `wsl.exe` client is attached to the running serve
  process, the VM has an active client and stays up.
- **(doc)** `wsl --shutdown` "Immediately terminates all running
  distributions and the WSL 2 lightweight utility virtual machine" and
  `wsl --terminate <Distribution>` stop a distribution — the explicit kill
  mechanisms for diagnostics [Basic commands for WSL](https://learn.microsoft.com/en-us/windows/wsl/basic-commands).

### 3.5 Quoting caveat for `cmd.exe` launches

**(live)** `cmd.exe /c` invoked from inside WSL prints a warning that the
current directory is a UNC path (`\\wsl.localhost\...`) and falls back to
the Windows directory. This affects shell-based launchers that call
`cmd.exe`; it does not affect the scheduled task, whose working directory
is set by Task Scheduler. It is a reason for the launcher/installer to
always pass absolute Windows paths or `cmd.exe /c start` with explicit
arguments.

## 4. WSL2 localhost forwarding

- **(doc)** "By default, WSL uses a NAT (Network Address Translation) based
  architecture" and, under "Accessing Linux networking apps from Windows
  (localhost)": "If you are building a networking app... in your Linux
  distribution, you can access it from a Windows app (like your Edge or
  Chrome internet browser) using localhost (just like you normally would)"
  [Accessing network applications with WSL](https://learn.microsoft.com/en-us/windows/wsl/networking).
  The WSL FAQ states it even more directly: "WSL shares the IP address of
  Windows... you can access any ports on localhost e.g. if you had web
  content on port 1234 you could https://localhost:1234 into your Windows
  browser" [WSL FAQ](https://learn.microsoft.com/en-us/windows/wsl/faq).
- **(doc)** Mirrored networking (`networkingMode=mirrored` in
  `.wslconfig`) is a non-default Windows 11 22H2+ option; NAT remains the
  default [Accessing network applications with WSL, Mirrored mode
  networking](https://learn.microsoft.com/en-us/windows/wsl/networking).
- **(live)** A listener bound **only to `127.0.0.1:8787`** inside WSL
  (`python3 -m http.server 8787 --bind 127.0.0.1`) was reachable from
  Windows with `curl.exe http://127.0.0.1:8787/` → HTTP `200`. This proves
  the launcher's ready check and the Windows browser can hit
  `http://127.0.0.1:8787/...` **without widening the WSL listener** — the
  listener stays `127.0.0.1:8787` exactly as PKG-009 requires.
- **(doc)** Firewall: on Windows 11 22H2+ with WSL 2.0.9+, the Hyper-V
  firewall is on by default; the documented command to permit inbound WSL
  traffic is `Set-NetFirewallHyperVVMSetting` / `New-NetFirewallHyperVRule`
  [Accessing network applications with WSL, WSL and firewall](https://learn.microsoft.com/en-us/windows/wsl/networking).
  The docs do not single out `localhost`/loopback in the firewall
  discussion; loopback forwarding is the documented default behavior and was
  observed working live without any firewall rule.
- The relay's ready endpoint is `GET /ready` → HTTP 200
  `{"status":"ready"}` and the management page is the web root `/`
  (project source: `src/server.rs:650` and `src/server.rs:706` — 
  [local](../../../src/server.rs)); the launcher checks `/ready` and opens
  `/`.

## 5. Console launcher mechanics

### 5.1 Opening the Windows default browser

- **(doc)** `cmd.exe /c start "" "<url>"` is the documented idiom:
  "You can run non-executable files through their file association...
  including URLs, which are automatically detected and opened in the
  default browser" and the example `start "Bing" "https://www.bing.com"`
  [start](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/start).
  The empty first argument supplies the required window *title* placeholder
  so the URL is not consumed as a title.
- **(doc)** `explorer.exe "<url>"` also opens via the shell, but the `start`
  command is the purpose-built, deterministic route (no window title side
  effects, documented URL handling). PowerShell `Start-Process "<url>"`
  works too but pulls in a PowerShell process; for a task/launcher context
  `start` (from a `.cmd`) or `explorer.exe` (from WSL interop) is simpler.
- **(gap/live)** The browser open was not exercised live (it would pop a
  window on the user's desktop); the mechanism is doc-backed only.

### 5.2 The ready check without extra installs

- **(doc)** `curl` is included with Windows: "curl is a command-line tool
  for transferring data to and from a server. It's included with
  Windows... The Windows version is built from the upstream curl project,
  so the same flags and behavior you know from Linux and macOS work the
  same way on Windows" — with the warning that Windows PowerShell 5.1
  aliases `curl` to `Invoke-WebRequest`, so launchers must call `curl.exe`
  explicitly [curl on Windows](https://learn.microsoft.com/en-us/windows/curl/).
- **(live)** `C:\Windows\System32\curl.exe` exists here; `curl.exe
  --version` → `curl 8.16.0 (Windows)`. (Note: `where curl` in this PATH
  also finds third-party curls from Anaconda/Git — the launcher should use
  `curl.exe` from System32 or rely on PATH order carefully.)
- **(live)** Ready-check semantics on this host:
  - service up: `curl.exe -s -o NUL -w "%{http_code}" http://127.0.0.1:8787/ready` → prints `200`, exit code `0`;
  - nothing listening: prints `000`, exit code `7` (connection refused);
  - a listener that answers anything else (e.g. a plain HTTP server) returns its real code (`404` observed) — so the launcher must match exactly `200`, and treat exit `7` / `000` / non-200 as "not ready".

## 6. Command-line limits and quoting

- **(doc)** `schtasks /tr` "path name must not exceed 262 characters"; `/tn`
  "must conform to the rules for file names, not exceeding 238 characters"
  [schtasks create, /tr and /tn](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/schtasks-create). The recommended `/TR` below is ~90 characters — no limit concern.
- **(live)** The `/TR` string is stored verbatim as the task's Command +
  Arguments and round-trips exactly into `Task To Run` in `/Query /V /FO
  LIST` (§1.3). The local `schtasks.exe /Create /?` help documents the
  double-quoting rule for paths with spaces (two sets of quotes: one for
  CMD, one for `schtasks.exe`). Because the recommended `/TR` contains no
  spaces, no quoting is required at all.
- **(doc/live)** `wsl.exe` argument passing: `--` forwards the remainder
  as-is and `-e` executes without the default Linux shell (local `--help`,
  §3.1); both were verified live with multiple arguments. `~` is not valid
  here: the task runs from Windows, and `~` would depend on shell/default-
  user resolution inside WSL — the `/TR` must use an absolute WSL path.
  This matches the ticket note "`~/.local/bin/local-api-relay` resolves
  inside WSL home; the task runs from Windows so the `/TR` must not use
  `~`".

## 7. ONLOGON trigger semantics: not before logon, and after exhaustion

- **(doc)** The logon trigger is per-user by construction: "a trigger that
  starts a task when a user logs on. When the Task Scheduler service
  starts, all logged-on users are enumerated and any tasks registered with
  logon triggers that match the logged on user are run" [LogonTrigger
  object](https://learn.microsoft.com/en-us/windows/win32/taskschd/logontrigger).
- **(doc)** The "not before the user logs on" property is enforced by the
  principal: `InteractiveToken` means "User must already be logged on. The
  task will be run only in an existing interactive session" [LogonType
  element](https://learn.microsoft.com/en-us/windows/win32/taskschd/taskschedulerschema-logontype-principaltype-element).
  There is no interactive session for the user before logon, so the task
  cannot fire. (PKG-006's "must not depend on WSL systemd" is satisfied by
  construction: the task runs `wsl.exe` directly and does not touch
  systemd.)
- **(live)** For the per-user ONLOGON task, `Next Run Time` is `N/A` between
  logons (logon triggers only have a next run when a matching logon
  happens). After the bounded restart bound is reached (or any failure),
  the task simply shows its last failed result and waits for the next
  logon trigger; the live runs in §2.3 showed exactly this "stays failed,
  no loop" end-state. No Microsoft Learn page describes the failure case
  explicitly **(gap)**, but the "waits for the next trigger" behavior is
  the only one consistent with the trigger semantics above and the
  observed Last Result/Next Run Time fields.

## 8. SEC-005 cross-check for the four surfaces

- **Scheduled task:** `InteractiveToken` principal → no password stored
  (§1.4, **(live)** XML export contains no credential material). The action
  is only `wsl.exe ... -- /home/<user>/.local/bin/local-api-relay serve` —
  no secret.
- **Process environment:** `wsl.exe` passes no environment from the task
  beyond the normal Windows environment; nothing secret is in the `/TR`.
  The lifecycle script already documents "No secret — including the
  administrator bootstrap credential — is ever written to a script, the
  environment, or the logs (SEC-005)" (project source:
  `packaging/local-api-relay-service`, header comment — [local](../../../packaging/local-api-relay-service)).
- **Normal logs:** the launcher prints only status text and diagnostic
  commands (§5), never credentials; the relay's own logs are metadata-only
  per OPS-020.
- **Launcher:** it contains the ready-check URL, the management URL, and
  fixed diagnostic commands; nothing secret (SEC-005). It must not embed
  the bootstrap credential; the ready endpoint is unauthenticated and
  loopback-only, which is fine for a *local* readiness probe.

## 9. Recommended concrete shape for ticket 30

### (a) Task creation — ship a task XML template and import it

Recommended (verified live end-to-end on this host):

```xml
<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>local-api-relay login task</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <UserId>LAPTOP-45RC44AF\user</UserId>   <!-- per-user, NOT any-user -->
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>LAPTOP-45RC44AF\user</UserId>   <!-- resolved to SID on registration -->
      <LogonType>InteractiveToken</LogonType> <!-- run only when logged on; no password -->
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <RestartOnFailure>
      <Count>3</Count>
      <Interval>PT1M</Interval>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>wsl.exe</Command>
      <Arguments>-d Ubuntu -u <user> -- /home/<user>/.local/bin/local-api-relay serve</Arguments>
    </Exec>
  </Actions>
</Task>
```

Installer call (idempotent via `/F`; file encoded UTF-16LE with BOM,
passed as a Windows path — the `\\wsl.localhost\...` UNC works):

```
schtasks.exe /Create /TN "local-api-relay" /XML "<windows-path-to-xml>" /F
```

Equivalent PowerShell one-liner (also verified live) if the installer
prefers it:

```powershell
$p  = New-ScheduledTaskPrincipal -UserId "LAPTOP-45RC44AF\user" -LogonType Interactive -RunLevel Limited
$t  = New-ScheduledTaskTrigger -AtLogOn -User "LAPTOP-45RC44AF\user"
$s  = New-ScheduledTaskSettingsSet -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit (New-TimeSpan -Seconds 0)
$a  = New-ScheduledTaskAction -Execute "wsl.exe" -Argument "-d Ubuntu -u <user> -- /home/<user>/.local/bin/local-api-relay serve"
Register-ScheduledTask -TaskName "local-api-relay" -Principal $p -Trigger $t -Settings $s -Action $a -Force
```

**Do not** use `schtasks /SC ONLOGON` without the XML: for a standard user
it produces an any-user logon trigger and was access-denied in this
environment (§1.1). The installer should substitute the actual machine
name/user via `cmd.exe /c whoami` or `$env:USERDOMAIN\$env:USERNAME`
(verified available here), never hard-code a bootstrap credential.

### (b) Bounded restart setting

`RestartOnFailure` in the XML (or `-RestartCount 3 -RestartInterval
(New-TimeSpan -Minutes 1)` in PowerShell), per §2.2. Both `Count` and
`Interval` must be present. After the bound, the task remains failed and
waits for the next logon (verified live — no infinite loop). If an
installed task ever needs the settings changed, modify the object and
`Set-ScheduledTask -InputObject $t` (preserves other settings; `schtasks
/Change` prompts for a password here and is not recommended).

### (c) The `/TR` string

```
wsl.exe -d Ubuntu -u <user> -- /home/<user>/.local/bin/local-api-relay serve
```

- Distro and WSL user pinned (`-d`, `-u`) so default-distro/default-user
  changes do not redirect the task (§3.3).
- Absolute WSL path, no `~` (§6).
- `--` forwards `serve` as-is; `-e` is an equivalent alternative but `--`
  is simpler to embed in `/TR` and both were verified with multi-argument
  commands (§3.1).
- The long-running `wsl.exe` keeps the WSL2 VM alive while `serve` runs
  (§3.2, FAQ lifetime quote) and propagates the serve exit code into the
  task's Last Result for codes 0–255 (§3.2).

### (d) Desktop console launcher

`local-api-relay-launcher.cmd` (or equivalent), logic:

```
curl.exe -s -o NUL -w "%{http_code}" http://127.0.0.1:8787/ready
```

- If the result is exactly `200` → `cmd.exe /c start "" "http://127.0.0.1:8787/"`.
- Otherwise (exit code `7` / `000` / non-200, §5.2) → print status and
  actionable diagnostics, for example:
  - `schtasks.exe /Query /TN "local-api-relay" /V /FO LIST` (is the login
    task registered/enabled; Last Result);
  - `wsl.exe -d Ubuntu -u <user> -- /home/<user>/.local/bin/local-api-relay-service status`
    (is the service running inside WSL);
  - `wsl.exe -d Ubuntu -u <user> -- /home/<user>/.local/bin/local-api-relay-service start`
    (start it now);
  - `wsl.exe --status` (is WSL healthy).
- Use `curl.exe` explicitly (PowerShell 5.1 aliases `curl` to
  `Invoke-WebRequest`, §5.2); the launcher carries no secrets (§8).

## 10. Source gaps

1. **`wsl.exe` exit-code propagation** is not documented in Microsoft
   Learn. Live-verified: 0–255 exact, >255 wraps mod 256. Do not rely on
   the wrapping behavior; keep serve exit codes in 0–255.
2. **Restart-on-failure firing conditions** are not documented. The
   settings are documented, but in this environment the failing task was
   never visibly re-launched within 4–4.5 minutes across three
   experiments (Interactive principal; demand-run and Once-trigger;
   PowerShell and XML creation). Whether non-interactive (S4U/Password)
   principals restart could not be tested (S4U registration denied from
   the WSL interop token; Password requires credentials). The bounded /
   no-infinite-loop property was verified live.
3. **`wsl.exe` child behavior at Windows logoff** is not documented; the
   design must not depend on it (and does not — the task only runs while
   the user is logged on).
4. **Windows Firewall and `localhost`**: the WSL docs cover Hyper-V
   firewall defaults but not loopback specifically; loopback forwarding was
   verified live with no firewall changes.
5. **Browser open** was not exercised live (side effect on the user's
   desktop); `start "" "<url>"` is doc-backed.
6. The `/Query /V /FO LIST` field labels are localized (Chinese here);
   English labels come from the Learn reference's example output.

## Evidence log (commands run live on 2026-08-11)

All Windows processes were invoked from WSL2 interop. Throwaway tasks
(`larr-*`) were deleted immediately after each experiment; the machine's
pre-existing tasks and services were only queried, never modified.

- `schtasks.exe /Create /?`, `/Query /?`, `/Change /?` (captured, GBK→UTF-8)
- `wsl.exe --help`, `wsl.exe --list --verbose`, `wsl.exe --status`
- `schtasks.exe /Create /SC ONLOGON|ONSTART|ONCE ...` (denial/success matrix)
- `schtasks.exe /Create /TN larr-research-xml /XML <file> /F` → created;
  `/Query /TN ... /V /FO LIST` and `/XML` round-trip; `/Run` (Last Result 0)
  then failing action (Last Result 1); polled 3.5 min — no restart
- PowerShell `Register-ScheduledTask` matrix: `-AtLogOn`/`-AtStartup`
  without `-User` → 0x80070005; `-AtLogOn -User` → OK; `-Once` → OK;
  S4U principal → 0x80070005
- Trigger-based restart experiments (`larr-restart-trig` PowerShell,
  `larr-restart-xml` XML import): failing action, `RestartOnFailure
  Count=3/PT1M`, polled 4–4.5 min — no re-launch, Last Result stayed 1
- `cmd.exe /c wsl.exe -d Ubuntu -- echo ok`; `-e`; `-u root`; multi-arg;
  `sh -c 'exit 7'` → 7; `exit 300` → 44; `sleep 2` → 2.13 s attached
- WSL listener `python3 -m http.server 8787 --bind 127.0.0.1`; from
  Windows `curl.exe` → HTTP 200 (also 404 and 000/exit-7 cases)
- `curl.exe --version` → 8.16.0; `Get-ScheduledTaskInfo` for the test tasks;
  existing tasks `\OpenClaw Gateway` and `\OneDrive Startup Task-...`
  queried for LogonTrigger / RestartOnFailure evidence
