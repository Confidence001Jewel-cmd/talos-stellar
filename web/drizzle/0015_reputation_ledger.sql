CREATE TABLE IF NOT EXISTS "tls_reputation_inputs" (
	"id" text PRIMARY KEY NOT NULL,
	"talosId" text NOT NULL,
	"jobId" text NOT NULL,
	"requesterTalosId" text NOT NULL,
	"status" text NOT NULL,
	"jobCreatedAt" timestamp (3) NOT NULL,
	"jobUpdatedAt" timestamp (3),
	"deadlineAt" timestamp (3),
	"refundAmount" numeric(18, 6),
	"hasResult" boolean DEFAULT false NOT NULL,
	"txHash" text,
	"createdAt" timestamp (3) DEFAULT now() NOT NULL,
	"updatedAt" timestamp (3) NOT NULL,
	CONSTRAINT "tls_reputation_inputs_jobId_unique" UNIQUE("jobId")
);
--> statement-breakpoint
DO $$ BEGIN
 ALTER TABLE "tls_reputation_inputs" ADD CONSTRAINT "tls_reputation_inputs_talosId_tls_talos_id_fk" FOREIGN KEY ("talosId") REFERENCES "public"."tls_talos"("id") ON DELETE cascade ON UPDATE no action;
EXCEPTION
 WHEN duplicate_object THEN null;
END $$;
--> statement-breakpoint
CREATE INDEX IF NOT EXISTS "tls_reputation_inputs_talosId_jobCreatedAt_idx" ON "tls_reputation_inputs" USING btree ("talosId","jobCreatedAt");--> statement-breakpoint
CREATE INDEX IF NOT EXISTS "tls_reputation_inputs_talosId_requester_idx" ON "tls_reputation_inputs" USING btree ("talosId","requesterTalosId");
