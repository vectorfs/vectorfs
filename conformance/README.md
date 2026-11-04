# conformance

Dual first-party SDK checks of the protocol face (design.md §3 core
acceptance): `boto3_check.py` and `go/main.go` (aws-sdk-go-v2) drive the same
32-step matrix 1:1 (core 9 / management 9 / data plane 7 / filters 6 /
cleanup 1); any FAIL exits 1. Both talk to an already-running server:

```bash
vectorfs init --data-dir ./vectorfs-data && vectorfs serve --data-dir ./vectorfs-data &  # creds printed by init
python3 conformance/boto3_check.py --access-key "$AK" --secret-key "$SK"
(cd conformance/go && go run . --access-key "$AK" --secret-key "$SK")
```

Options on both: `--endpoint` (default http://127.0.0.1:8787), `--region`,
`--prefix` (resource-name isolation for concurrent runs). `vectorfs serve
--backend memory --no-auth` skips SigV4 for an auth-free run. The same
commands run against ANY endpoint — including real AWS (`--endpoint` unset +
your cloud credentials via the default chain) once available: every step
cleans up after itself, so it is safe against a real account. CI runs both on
every push.
