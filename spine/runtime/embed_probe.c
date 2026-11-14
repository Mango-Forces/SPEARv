/* A runtime ABI probe, not the SPEARv server or an array ownership proof. */
#include <Python.h>
#include <stdio.h>

int main(void) {
    PyConfig config;
    PyConfig_InitIsolatedConfig(&config);
    config.site_import = 0;
    config.user_site_directory = 0;
    config.use_environment = 0;
    config.write_bytecode = 0;
    config.module_search_paths_set = 1;
    PyStatus status = PyConfig_SetString(&config, &config.home, L"/opt/spearv/python");
    if (!PyStatus_Exception(status))
        status = PyWideStringList_Append(&config.module_search_paths, L"/opt/spearv/python/lib/python3.14");
    if (!PyStatus_Exception(status))
        status = PyWideStringList_Append(&config.module_search_paths, L"/opt/spearv/python/lib/python3.14/lib-dynload");
    if (!PyStatus_Exception(status))
        status = PyWideStringList_Append(&config.module_search_paths, L"/opt/spearv/python/lib/python3.14/site-packages");
    if (!PyStatus_Exception(status))
        status = Py_InitializeFromConfig(&config);
    if (PyStatus_Exception(status)) {
        fprintf(stderr, "private Python initialization failed: %s\n", status.err_msg);
        PyConfig_Clear(&config);
        return 2;
    }
    PyConfig_Clear(&config);
    FILE *script = fopen("/app/preflight.py", "r");
    if (!script) { perror("private preflight"); return 3; }
    int result = PyRun_SimpleFileExFlags(script, "/app/preflight.py", 1, NULL);
    if (Py_FinalizeEx() < 0) return 4;
    return result == 0 ? 0 : 5;
}
