-- v0.12 SYNC metadata delta for namespaces: stamp NS-META changes with the
-- global seq so a reconnecting member catches up title/icon/category/
-- visibility/recovery changes missed while offline (task 21). Reuses the
-- weft_stamp_seq trigger from migration 0036.
ALTER TABLE weft_namespaces ADD COLUMN seq BIGINT;
UPDATE weft_namespaces SET seq = nextval('weft_seq');
CREATE INDEX weft_namespaces_seq ON weft_namespaces (seq);
CREATE TRIGGER weft_namespaces_stamp
    BEFORE INSERT OR UPDATE ON weft_namespaces
    FOR EACH ROW EXECUTE FUNCTION weft_stamp_seq();
