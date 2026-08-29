/* rua — a small Lua-shaped language for Rust.
 *
 * Link against librua.so (cdylib):
 *   cc demo/embed.c -I include -L target/release -lrua -o embed
 *
 * Every function is thread-confined: use one rua_State per thread.
 */
#ifndef RUA_H
#define RUA_H

#ifdef __cplusplus
extern "C" {
#endif

typedef struct RuaState rua_State;

/* A C function exposed to scripts: numbers in, one number out. */
typedef double (*rua_NumFn)(const double *args, int n);

/* lifecycle */
rua_State *rua_new(void);
void       rua_close(rua_State *S);

/* running code: 0 on success, -1 on error */
int         rua_eval(rua_State *S, const char *src);
int         rua_dofile(rua_State *S, const char *path);
const char *rua_error(rua_State *S);

/* results of the last eval/call */
int         rua_result_count(rua_State *S);
double      rua_result_number(rua_State *S, int i);
const char *rua_result_string(rua_State *S, int i);

/* globals */
void   rua_set_number(rua_State *S, const char *name, double v);
void   rua_set_string(rua_State *S, const char *name, const char *v);
double rua_get_number(rua_State *S, const char *name);

/* calling into rua, and exposing C to rua */
int  rua_call(rua_State *S, const char *name, const double *args, int n, double *out);
void rua_register(rua_State *S, const char *name, rua_NumFn f);

/* 1 = compile hot functions with rustc, 0 = always interpret */
void rua_jit(rua_State *S, int on);

#ifdef __cplusplus
}
#endif
#endif /* RUA_H */
