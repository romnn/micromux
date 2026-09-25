#!/bin/sh
printf '\033[1;36m[api]\033[0m starting micromux demo api \033[2mv1.4.2\033[0m\n'
printf '\033[2m→\033[0m connecting to postgres://localhost:5432 \033[32m✓\033[0m\n'
printf '\033[2m→\033[0m connecting to redis://localhost:6379 \033[32m✓\033[0m\n'
printf '\033[32m●\033[0m \033[1mlistening on http://0.0.0.0:8080\033[0m\n'
printf '\033[90m12:00:01\033[0m \033[32mGET   \033[0m /health        \033[32m200\033[0m   1ms\n'
printf '\033[90m12:00:01\033[0m \033[32mGET   \033[0m /api/users     \033[32m200\033[0m  12ms\n'
printf '\033[90m12:00:02\033[0m \033[36mPOST  \033[0m /api/orders    \033[32m201\033[0m  34ms\n'
printf '\033[90m12:00:02\033[0m \033[32mGET   \033[0m /api/orders/9  \033[33m404\033[0m   2ms\n'
printf '\033[90m12:00:03\033[0m \033[32mGET   \033[0m /api/products  \033[32m200\033[0m   8ms\n'
printf '\033[90m12:00:03\033[0m \033[33mPUT   \033[0m /api/cart/42   \033[32m200\033[0m  17ms\n'
printf '\033[90m12:00:04\033[0m \033[31mDELETE\033[0m /api/cart/42   \033[32m204\033[0m   9ms\n'
printf '\033[90m12:00:04\033[0m \033[36mPOST  \033[0m /api/login     \033[32m200\033[0m  21ms\n'
exec sleep 1000000
