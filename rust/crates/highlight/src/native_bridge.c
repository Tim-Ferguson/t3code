/* Owned ABI adapter for the pinned Oniguruma dependency. */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include "oniguruma.h"

int t3_onig_initialize(void) {
  OnigEncoding encoding = ONIG_ENCODING_UTF8;
  return onig_initialize(&encoding, 1);
}

static void describe_error(int code, OnigErrorInfo *info, char *error) {
  if (info) onig_error_code_to_str((OnigUChar *)error, code, info);
  else onig_error_code_to_str((OnigUChar *)error, code);
}

void *t3_onig_compile(const uint8_t *const *patterns, const size_t *lengths,
                      int count, char *error) {
  OnigRegex *regs = calloc(count ? (size_t)count : 1, sizeof(OnigRegex));
  OnigRegSet *set = NULL;
  if (!regs) { strcpy(error, "scanner allocation failed"); return NULL; }
  int compiled = 0;
  for (; compiled < count; compiled++) {
    OnigErrorInfo info;
    int code = onig_new(&regs[compiled], patterns[compiled],
      patterns[compiled] + lengths[compiled], ONIG_OPTION_CAPTURE_GROUP,
      ONIG_ENCODING_UTF8, ONIG_SYNTAX_DEFAULT, &info);
    if (code != ONIG_NORMAL) {
      describe_error(code, &info, error);
      goto failure;
    }
  }
  int code = onig_regset_new(&set, count, regs);
  if (code != ONIG_NORMAL) {
    describe_error(code, NULL, error);
    goto failure;
  }
  free(regs); /* The set now owns every compiled regex. */
  return set;
failure:
  for (int i = 0; i < compiled; i++) onig_free(regs[i]);
  free(regs);
  return NULL;
}

void t3_onig_free(void *scanner) { onig_regset_free(scanner); }

/* Return 1 for a match, 0 for no match, -1 for an error. Captures are borrowed
   until the next search on this owned scanner, and copied by Rust immediately. */
int t3_onig_find(void *scanner, const uint8_t *text, size_t length, size_t start,
                 uint32_t options, int *pattern, int *count,
                 const int **beg, const int **end, char *error) {
  if (start > length) return 0;
  if (onig_regset_number_of_regex(scanner) == 0) return 0;
  OnigOptionType flags = ONIG_OPTION_NONE;
  if (options & 1) flags |= ONIG_OPTION_NOT_BEGIN_STRING;
  if (options & 2) flags |= ONIG_OPTION_NOT_END_STRING;
  if (options & 4) flags |= ONIG_OPTION_NOT_BEGIN_POSITION;
  int location = 0;
  int code = onig_regset_search(scanner, text, text + length, text + start,
    text + length, ONIG_REGSET_POSITION_LEAD, flags, &location);
  if (code == ONIG_MISMATCH) return 0;
  if (code < 0) { describe_error(code, NULL, error); return -1; }
  OnigRegion *region = onig_regset_get_region(scanner, code);
  *pattern = code; *count = region->num_regs;
  *beg = region->beg; *end = region->end;
  return 1;
}
