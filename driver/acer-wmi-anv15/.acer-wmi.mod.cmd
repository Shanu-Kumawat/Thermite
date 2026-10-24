savedcmd_acer-wmi.mod := printf '%s\n'   acer-wmi.o | awk '!x[$$0]++ { print("./"$$0) }' > acer-wmi.mod
