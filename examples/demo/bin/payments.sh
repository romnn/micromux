#!/bin/sh
printf '\033[1;33m[payments]\033[0m booting payment gateway \033[2mv2.1\033[0m\n'
printf '\033[2m→\033[0m loading merchant config \033[2m(3 accounts)\033[0m\n'
printf '\033[2m→\033[0m dialing acquirer at 127.0.0.1:7070\n'
printf '\033[31m✗\033[0m connect: connection refused\n'
printf '\033[2m→\033[0m retry 1/5 in 5s\n'
printf '\033[2m→\033[0m retry 2/5 in 5s\n'
printf '\033[33m!\033[0m acquirer unreachable, marking degraded\n'
exec sleep 1000000
