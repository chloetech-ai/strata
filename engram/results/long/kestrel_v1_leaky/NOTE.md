# First kestrel run: not valid as a comparison

These 210 runs are kept for the record only. Claude Code's own auto memory
(`~/.claude/projects/<repo>/memory/`) lives outside the repository, and the
harness did not reset it between runs. Notes one task run wrote there were
read by the runs after it in the same lane: many answers mention a "price
memory note" an earlier appointment_price run had written. Runs within a
lane were therefore not independent, and the order of tasks mattered.

The invoice_export grader also crashed on repositories whose Invoice had
dropped patient_name during the history (the privacy rule), scoring 0
without looking at the export.

long_session_ab.py now snapshots that memory at the end of the history and
restores it before every run, and the grader builds dataclasses by keyword.
The rerun is in ../kestrel.
