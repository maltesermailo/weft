-- §6.2 welcome channel: the channel that receives a system "welcome" line when
-- a new member joins the namespace. NULL = no welcome message.
ALTER TABLE weft_namespaces ADD COLUMN welcome_channel TEXT;
