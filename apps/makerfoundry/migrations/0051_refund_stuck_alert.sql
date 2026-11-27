-- Issue #167: a Dashboard refund still pending or waiting on the buyer after
-- `refunds::STUCK_AFTER_DAYS` days alerts the admins once; the order stays on the sweep.
ALTER TABLE mf_orders ADD COLUMN refund_stuck_alerted_at TIMESTAMPTZ;
