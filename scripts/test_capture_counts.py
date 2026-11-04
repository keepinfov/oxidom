"""Literal regression for the VM capture gate, without booting its driver."""

import ast
from pathlib import Path
import unittest


class CaptureCounts(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        source_path = Path(__file__).with_name("routing-vm-faults.py")
        source = ast.parse(source_path.read_text(), filename=str(source_path))
        definitions = [
            node for node in source.body
            if isinstance(node, ast.FunctionDef)
            and node.name == "fault_capture_reports_zero_kernel_drops"
        ]
        if len(definitions) != 1:
            raise AssertionError("the actual capture gate must have one self-contained helper")
        # Execute only the function used by the real closed-capture path.
        # The VM script's imports, scenarios and driver globals stay unexecuted.
        namespace = {}
        helper = ast.Module(body=definitions, type_ignores=[])
        exec(compile(helper, str(source_path), "exec"), namespace)
        cls.check = staticmethod(namespace[definitions[0].name])

    def test_exact_zero_kernel_drop_reports_are_accepted(self):
        journals = [
            "0 packets dropped by kernel\n",
            "  0 packets dropped by kernel  \n",
            "Nov 04 10:20:00 client tcpdump[1020]: 0 packets dropped by kernel\n",
            "Nov 04 10:20:00 client systemd[1]: Started fixture capture.\n"
            "Nov 04 10:20:01 client tcpdump[1020]: 0 packets dropped by kernel\n"
            "Nov 04 10:20:01 client systemd[1]: Stopped fixture capture.\n",
        ]
        for journal in journals:
            with self.subTest(journal=journal):
                self.assertTrue(self.check(journal))

    def test_nonzero_kernel_drop_counts_never_prove_a_complete_capture(self):
        journals = [
            "10 packets dropped by kernel\n",
            "20 packets dropped by kernel\n",
            "100 packets dropped by kernel\n",
            "Nov 04 10:20:00 client tcpdump[1020]: 10 packets dropped by kernel\n",
            "Nov 04 10:20:00 client tcpdump[1020]: 20 packets dropped by kernel\n",
        ]
        for journal in journals:
            with self.subTest(journal=journal):
                self.assertFalse(self.check(journal))

    def test_missing_or_misleading_text_cannot_establish_zero_kernel_drops(self):
        journals = [
            "",
            "Nov 04 10:20:00 client systemd[1]: Stopped fixture capture.\n",
            "Nov 04 10:20:00 client tcpdump[1020]: 25 packets received by filter\n",
            "Nov 04 10:20:00 client tcpdump[1020]: note '0 packets dropped by kernel'\n",
            "Nov 04 10:20:00 client tcpdump[1020]: 0 packets dropped by kernel (unverified)\n",
            "Nov 04 10:20:00 client tcpdump[1020]: -10 packets dropped by kernel\n",
        ]
        for journal in journals:
            with self.subTest(journal=journal):
                self.assertFalse(self.check(journal))

    def test_multiple_summaries_cannot_hide_a_nonzero_or_ambiguous_capture(self):
        journals = [
            "0 packets dropped by kernel\n10 packets dropped by kernel\n",
            "20 packets dropped by kernel\n0 packets dropped by kernel\n",
            "0 packets dropped by kernel\n0 packets dropped by kernel\n",
            "Nov 04 10:20:00 client tcpdump[1020]: 0 packets dropped by kernel\n"
            "Nov 04 10:20:01 client tcpdump[1020]: 20 packets dropped by kernel\n",
        ]
        for journal in journals:
            with self.subTest(journal=journal):
                self.assertFalse(self.check(journal))


if __name__ == "__main__":
    unittest.main()
