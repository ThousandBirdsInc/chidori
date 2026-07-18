# Near-empty script used to measure CPython's process-startup baseline
# (binary load + interpreter setup), which the harness subtracts from the
# workload totals to estimate execution-only time. Twin of startup.js.
print("RESULT=ok")
