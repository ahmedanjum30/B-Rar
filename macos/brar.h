// B-Rar's engine (crates/brar-ffi), for the Mac app. See crates/brar-ffi/src/lib.rs for the
// details of each function; strings returned are freed with brar_string_free.
#include <stdbool.h>
#include <stdint.h>

typedef struct BrarJob BrarJob;

BrarJob *brar_list(const char *archive, const char *password);
BrarJob *brar_extract(const char *archive, const char *dest, const char *password, const char *selection_json, int32_t conflicts);
BrarJob *brar_create(const char *archive, const char *inputs_json, uint32_t level, const char *password, bool overwrite);

char *brar_job_status(BrarJob *job);
char *brar_job_take_entries(BrarJob *job);
void brar_job_answer_conflict(BrarJob *job, int32_t resolution, bool apply_to_all);
void brar_job_answer_part(BrarJob *job, const char *path);
void brar_job_set_paused(BrarJob *job, bool paused);
void brar_job_cancel(BrarJob *job);
void brar_job_free(BrarJob *job);

char *brar_suggested_dir(const char *archive);
bool brar_is_archive(const char *path);
void brar_string_free(char *s);
const char *brar_version(void);
