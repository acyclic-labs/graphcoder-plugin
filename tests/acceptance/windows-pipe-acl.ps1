# Asserts the running daemon's named pipe is reachable only by this account.
#
# The Unix socket is protected by living in a per-uid directory chmod'd to
# 0700. A Windows named pipe has no such parent to hide behind: it sits in a
# global namespace, and a pipe created with no security descriptor gets the
# system default, which grants read access to Everyone AND ANONYMOUS LOGON.
# The daemon on the other end restores files and rewinds trees on request, so
# `ipc::create_pipe_instance` builds an explicit owner-only DACL instead.
#
# This is here because that is exactly the kind of property that regresses
# without anyone noticing: the transport keeps working either way.
$ErrorActionPreference = 'Stop'

$pipes = @([System.IO.Directory]::GetFiles("\\.\pipe\") | Where-Object { $_ -match "acyclic" })
if ($pipes.Count -eq 0) { Write-Error "pipe-acl: no acyclic pipe found (is the daemon running?)"; exit 1 }

$name = [System.IO.Path]::GetFileName($pipes[0])
$client = New-Object System.IO.Pipes.NamedPipeClientStream(".", $name, [System.IO.Pipes.PipeDirection]::InOut)
try {
  $client.Connect(5000)
  $sddl = $client.GetAccessControl().GetSecurityDescriptorSddlForm('Access')
} finally {
  $client.Dispose()
}

"pipe-acl: $name"
"pipe-acl: $sddl"

# SDDL trustee aliases: WD = Everyone, AN = ANONYMOUS LOGON, IU = Interactive
# Users, BU = Builtin Users. Any of these on the daemon's endpoint means an
# account other than the owner can reach it.
$failed = $false
foreach ($trustee in 'WD', 'AN', 'IU', 'BU') {
  if ($sddl -match "\;$trustee\)") {
    Write-Error "pipe-acl: DACL grants '$trustee' access to the daemon pipe"
    $failed = $true
  }
}
if (-not ($sddl -match '^D:P')) {
  Write-Error "pipe-acl: DACL is not protected (expected to start with 'D:P'): $sddl"
  $failed = $true
}
if ($failed) { exit 1 }

"pipe-acl: owner-only, protected"
