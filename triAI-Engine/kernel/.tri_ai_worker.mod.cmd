savedcmd_tri_ai_worker.mod := printf '%s\n'   tri_ai_worker.o | awk '!x[$$0]++ { print("./"$$0) }' > tri_ai_worker.mod
