-- Add delivery_status column for tracking outgoing message publish state.
-- Values: NULL (incoming), "Sending", {"Sent":N}, {"Failed":"reason"}
ALTER TABLE aggregated_messages ADD COLUMN delivery_status TEXT DEFAULT NULL;
