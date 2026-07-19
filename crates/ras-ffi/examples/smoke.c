/*
 * Casual RAS SDK — C smoke consumer (ADR-096).
 *
 * Proves the C ABI works from real C (not just Rust calling the extern fns): generate an identity,
 * export its public key, sign + verify a message, render its contact code, and build ⇄ parse a ticket.
 *
 * Build (after `cargo build -p ras-ffi`, which emits the header + the library):
 *   cc crates/ras-ffi/examples/smoke.c -I crates/ras-ffi/include -L target/debug -lras_ffi -o smoke
 *   # macОS/Linux: run with the library on the loader path
 *   DYLD_LIBRARY_PATH=target/debug ./smoke     # (LD_LIBRARY_PATH=target/debug on Linux)
 */
#include "casual_ras.h"

#include <stdint.h>
#include <stdio.h>
#include <string.h>

int main(void) {
  printf("Casual RAS SDK version: %s\n", casual_ras_version());

  struct CasualRasIdentity *id = NULL;
  if (casual_ras_identity_generate(&id) != CASUAL_RAS_STATUS_OK) {
    fprintf(stderr, "identity_generate failed\n");
    return 1;
  }

  uint8_t pk[32];
  if (casual_ras_identity_public_key(id, pk) != CASUAL_RAS_STATUS_OK) {
    fprintf(stderr, "public_key failed\n");
    return 1;
  }

  const char *msg = "hello from C";
  uint8_t sig[64];
  if (casual_ras_identity_sign(id, (const uint8_t *)msg, strlen(msg), sig) !=
      CASUAL_RAS_STATUS_OK) {
    fprintf(stderr, "sign failed\n");
    return 1;
  }

  int verified = casual_ras_verify(pk, (const uint8_t *)msg, strlen(msg), sig);
  printf("signature verifies: %s\n", verified == CASUAL_RAS_STATUS_OK ? "yes" : "NO");

  char code[80];
  casual_ras_contact_code(pk, code, sizeof(code));
  printf("contact code: %s\n", code);

  char ticket[256];
  casual_ras_ticket_from_endpoint_id(pk, ticket, sizeof(ticket));

  uint8_t parsed[32];
  casual_ras_ticket_endpoint_id(ticket, parsed);
  int round_trips = (memcmp(pk, parsed, 32) == 0);
  printf("ticket build/parse round-trips: %s\n", round_trips ? "yes" : "NO");

  casual_ras_identity_free(id);

  int ok = (verified == CASUAL_RAS_STATUS_OK) && round_trips;
  printf("%s\n", ok ? "SDK smoke: OK" : "SDK smoke: FAILED");
  return ok ? 0 : 2;
}
