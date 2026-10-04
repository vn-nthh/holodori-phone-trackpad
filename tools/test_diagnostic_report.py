import unittest
from diagnostic_report import frame_report, join_report, parse_android


class CorrelationTest(unittest.TestCase):
    def test_repair_winner_uses_actual_winning_attempt_not_ordinal_guess(self):
        phone = [(21, 1000, 7, 9, 990, 10, 1, 0, 1000, 0, 0, 0),
                 (21, 3000, 7, 9, 2990, 10, 3, 0, 3000, 0, 0, 0),
                 (23, 4000, 7, 9, 10, 1, 2, 1, 990, 3, 1, 9)]
        host = [(1, 40, 7, 9, 1, 2, 2990, 0, 0, 1, 0, 1),
                (2, 50, 7, 9, 41, 0, 0, 0, 0, 0, 0, 0)]
        result = frame_report((7, 9), phone, host)
        self.assertEqual(result["winning_copy"], "repair")
        self.assertEqual(result["timing_ns"]["host_sink_attempts"], 9)
        self.assertEqual(result["timing_ns"]["phone_first_attempt_to_ack"], 3010)

    def test_sender_discard_is_not_assumed_lost_if_host_committed(self):
        frame = frame_report((7, 1), [(24, 100, 7, 1, 1, 1, 1, 1, 0, 0, 0, 0)],
                             [(2, 10, 7, 1, 9, 0, 0, 0, 0, 0, 0, 0)])
        self.assertEqual(frame["outcome"], "OS accepted")
        self.assertTrue(any("sender abandoned" in f for f in frame["findings"]))

    def test_missing_evidence_never_proves_packet_loss(self):
        frame = frame_report((7, 1), [(24, 100, 7, 1, 1, 1, 1, 1, 0, 0, 0, 0)], [])
        self.assertEqual(frame["outcome"], "sender discarded; host outcome unknown")

    def test_coarse_attempt_clock_does_not_fabricate_a_copy_winner(self):
        phone = [(21, 1000, 7, 1, 900, 100, ordinal, 0, 1000, 0, 0, 0) for ordinal in (1, 2)]
        host = [(1, 100, 7, 1, 1, 2, 900, 0, 0, 1, 0, 1)]
        self.assertEqual(frame_report((7, 1), phone, host)["winning_copy"], "ambiguous timestamp")

    def test_signed_android_ids_match_unsigned_host(self):
        header = "kind,at_ns,session,sequence,a,b,c,d,e,f,g,h\n"
        events, _, _ = parse_android({"a.csv": header + "20,100,-1,2,1,2,1,1,64,0,0,0\n" * 2})
        self.assertEqual(len(events), 1)
        result = join_report([], {"a.csv": header + "20,100,-1,2,1,2,1,1,64,0,0,0\n"})
        self.assertEqual(result["retained_frames"][0]["session"], "ffffffffffffffff")


if __name__ == "__main__":
    unittest.main()
