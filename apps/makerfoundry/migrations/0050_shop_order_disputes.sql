-- Issue #99: chargebacks on orders from shops, as #90 did for designs (0048). A buyer's dispute
-- (`charge.dispute.created`) pauses the order (`disputed`), takes the shop's share back from its
-- transfer and gives MakerFoundry's 10% of it back to the shop (the sales tax stays
-- MakerFoundry's). When the dispute closes: won (or an inquiry closed), the order goes back to
-- the status it had (`dispute_prev_status`) and the shop gets back what was taken, less the 10%
-- it had back; lost, the order is a `chargeback` and stays reversed. Each Stripe step is recorded
-- once (`*_at`), checked with Stripe first, and retried by the sweep until done.
ALTER TABLE mf_shop_orders DROP CONSTRAINT mf_shop_orders_status_check;
ALTER TABLE mf_shop_orders ADD CONSTRAINT mf_shop_orders_status_check
    CHECK (status IN ('pending', 'paid', 'sent', 'received', 'proof_sent', 'approved', 'in_production',
        'ready', 'shipped', 'collected', 'cancelled', 'refunded', 'expired', 'abandoned',
        'disputed', 'chargeback'));

ALTER TABLE mf_shop_orders ADD COLUMN dispute_id TEXT;
ALTER TABLE mf_shop_orders ADD COLUMN dispute_cents BIGINT NOT NULL DEFAULT 0 CHECK (dispute_cents >= 0);
ALTER TABLE mf_shop_orders ADD COLUMN dispute_outcome TEXT NOT NULL DEFAULT '';
ALTER TABLE mf_shop_orders ADD COLUMN dispute_prev_status TEXT NOT NULL DEFAULT '';
ALTER TABLE mf_shop_orders ADD COLUMN disputed_at TIMESTAMPTZ;
ALTER TABLE mf_shop_orders ADD COLUMN dispute_closed_at TIMESTAMPTZ;
ALTER TABLE mf_shop_orders ADD COLUMN dispute_reversed_at TIMESTAMPTZ;
ALTER TABLE mf_shop_orders ADD COLUMN dispute_fee_returned_at TIMESTAMPTZ;
ALTER TABLE mf_shop_orders ADD COLUMN dispute_restored_at TIMESTAMPTZ;
-- the last state the shop was emailed about (`disputed`, `won`, `lost`), and when the admins were
-- alerted that a Stripe step couldn't be done
ALTER TABLE mf_shop_orders ADD COLUMN dispute_mailed TEXT NOT NULL DEFAULT '';
ALTER TABLE mf_shop_orders ADD COLUMN dispute_alerted_at TIMESTAMPTZ;
CREATE INDEX mf_shop_orders_disputes ON mf_shop_orders (id) WHERE dispute_id IS NOT NULL;

ALTER TABLE mf_shop_ledger DROP CONSTRAINT mf_shop_ledger_reason_check;
ALTER TABLE mf_shop_ledger ADD CONSTRAINT mf_shop_ledger_reason_check
    CHECK (reason IN ('sale', 'refund', 'fee_return', 'dispute', 'dispute_fee', 'dispute_won'));
