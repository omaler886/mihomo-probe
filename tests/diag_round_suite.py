import sys
import time
import unittest

sys.path.insert(0, "/srv/mihomo-test")

import test_live

print("loading FullRoundTest ...", flush=True)
t0 = time.time()
suite = test_live.build_suite(["round"], False)
print(f"built suite in {time.time() - t0:.2f}s, tests={suite.countTestCases()}", flush=True)

# Run with a hard cap so we learn WHICH test hangs.
print("running with 240s cap ...", flush=True)


class GuardedResult(unittest.TextTestResult):
    pass


runner = unittest.TextTestRunner(verbosity=2)
t0 = time.time()
res = runner.run(suite)
print(f"done in {time.time() - t0:.1f}s ok={res.wasSuccessful()}", flush=True)
