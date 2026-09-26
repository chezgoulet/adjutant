
CREATE TABLE IF NOT EXISTS catalogue_items (
    id BIGSERIAL PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'product',
    sku TEXT,
    name TEXT NOT NULL,
    category TEXT NOT NULL DEFAULT 'other',
    description TEXT NOT NULL DEFAULT '',
    base_price_cents BIGINT NOT NULL,
    currency TEXT NOT NULL DEFAULT 'cad',
    fund_code TEXT NOT NULL DEFAULT '',
    equipment_item_id BIGINT,
    active BOOLEAN NOT NULL DEFAULT true,
    created_by TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT store_items_kind_valid CHECK (kind IN ('product', 'rental')),
    CONSTRAINT store_items_name_not_blank CHECK (btrim(name) <> ''),
    CONSTRAINT store_items_category_valid CHECK (category IN (
        'uniform', 'patch', 'insignia', 'gear', 'merch', 'other')),
    CONSTRAINT store_items_price_valid CHECK (
        base_price_cents >= 0 AND base_price_cents <= 100000000),
    CONSTRAINT store_items_rental_names_its_item CHECK (
        (kind = 'rental') = (equipment_item_id IS NOT NULL)),
    CONSTRAINT store_items_equipment_id_positive CHECK (
        equipment_item_id IS NULL OR equipment_item_id > 0)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_store_items_sku ON catalogue_items(sku)
  WHERE sku IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_store_items_kind ON catalogue_items(kind, active);
CREATE INDEX IF NOT EXISTS idx_store_items_category ON catalogue_items(category);

CREATE TABLE IF NOT EXISTS orders (
    id BIGSERIAL PRIMARY KEY,
    member_id TEXT NOT NULL,
    placed_by TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'open',
    currency TEXT NOT NULL DEFAULT 'cad',
    price_tier TEXT NOT NULL DEFAULT 'standard',
    price_cents BIGINT NOT NULL DEFAULT 0,
    charged_cents BIGINT NOT NULL DEFAULT 0,
    funded_cents BIGINT NOT NULL DEFAULT 0,
    fund_code TEXT NOT NULL DEFAULT '',
    note TEXT NOT NULL DEFAULT '',
    stripe_session_row BIGINT,
    stripe_session_id TEXT NOT NULL DEFAULT '',
    checkout_url TEXT NOT NULL DEFAULT '',
    payment_ref TEXT NOT NULL DEFAULT '',
    ledger_status TEXT NOT NULL DEFAULT '',
    ledger_transaction_id TEXT,
    ledger_error TEXT,
    completed_by TEXT NOT NULL DEFAULT '',
    completed_at TIMESTAMPTZ,
    comp_reason TEXT NOT NULL DEFAULT '',
    comp_by TEXT NOT NULL DEFAULT '',
    comp_at TIMESTAMPTZ,
    draw_status TEXT NOT NULL DEFAULT 'none',
    draw_ref TEXT NOT NULL DEFAULT '',
    draw_error TEXT,
    draw_by TEXT NOT NULL DEFAULT '',
    draw_attempted_at TIMESTAMPTZ,
    draw_booked_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT store_orders_member_present CHECK (btrim(member_id) <> ''),
    CONSTRAINT store_orders_status_valid CHECK (status IN (
        'open', 'awaiting_payment', 'paid', 'comped')),
    CONSTRAINT store_orders_tier_valid CHECK (price_tier IN (
        'patron', 'standard', 'supported', 'hardship')),
    CONSTRAINT store_orders_money_not_negative CHECK (
        price_cents >= 0 AND charged_cents >= 0 AND funded_cents >= 0),
    CONSTRAINT store_orders_charge_within_price CHECK (charged_cents <= price_cents),
    CONSTRAINT store_orders_funding_is_the_difference CHECK (
        funded_cents = price_cents - charged_cents),
    CONSTRAINT store_orders_draw_status_valid CHECK (draw_status IN (
        'none', 'unbooked', 'attempting', 'booked', 'refused', 'failed')),
    CONSTRAINT store_orders_draw_matches_funding CHECK (
        (funded_cents = 0) = (draw_status = 'none')),
    CONSTRAINT store_orders_draw_booked_has_reference CHECK (
        draw_status <> 'booked' OR (btrim(draw_ref) <> '' AND draw_booked_at IS NOT NULL)),
    CONSTRAINT store_orders_paid_is_charged CHECK (
        status <> 'paid' OR (charged_cents = price_cents AND charged_cents > 0
            AND btrim(payment_ref) <> '' AND completed_at IS NOT NULL)),
    CONSTRAINT store_orders_comp_is_an_authority CHECK (
        status <> 'comped' OR (charged_cents = 0 AND funded_cents = price_cents
            AND btrim(comp_reason) <> '' AND btrim(comp_by) <> '' AND comp_at IS NOT NULL)),
    CONSTRAINT store_orders_awaiting_has_a_session CHECK (
        status <> 'awaiting_payment' OR btrim(stripe_session_id) <> '')
);
CREATE INDEX IF NOT EXISTS idx_store_orders_member ON orders(member_id, id DESC);
CREATE INDEX IF NOT EXISTS idx_store_orders_status ON orders(status, id DESC);
CREATE INDEX IF NOT EXISTS idx_store_orders_draw ON orders(draw_status, id DESC);
CREATE INDEX IF NOT EXISTS idx_store_orders_comp ON orders(comp_at DESC)
  WHERE status = 'comped';

CREATE TABLE IF NOT EXISTS order_lines (
    id BIGSERIAL PRIMARY KEY,
    order_id BIGINT NOT NULL REFERENCES orders(id) ON DELETE CASCADE,
    catalogue_item_id BIGINT NOT NULL,
    item_name TEXT NOT NULL,
    item_kind TEXT NOT NULL DEFAULT 'product',
    fund_code TEXT NOT NULL DEFAULT '',
    equipment_item_id BIGINT,
    list_price_cents BIGINT NOT NULL,
    unit_price_cents BIGINT NOT NULL,
    quantity INTEGER NOT NULL,
    line_total_cents BIGINT NOT NULL,
    CONSTRAINT store_lines_kind_valid CHECK (item_kind IN ('product', 'rental')),
    CONSTRAINT store_lines_quantity_range CHECK (quantity > 0 AND quantity <= 100),
    CONSTRAINT store_lines_prices_valid CHECK (
        list_price_cents >= 0 AND unit_price_cents >= 0),
    CONSTRAINT store_lines_charged_within_price CHECK (unit_price_cents <= list_price_cents),
    CONSTRAINT store_lines_total_matches CHECK (line_total_cents = unit_price_cents * quantity),
    CONSTRAINT store_lines_rental_names_its_item CHECK (
        (item_kind = 'rental') = (equipment_item_id IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS idx_store_lines_order ON order_lines(order_id);
CREATE INDEX IF NOT EXISTS idx_store_lines_item ON order_lines(catalogue_item_id);