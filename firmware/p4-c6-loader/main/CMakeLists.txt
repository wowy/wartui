idf_component_register(SRCS "main.c" INCLUDE_DIRS "." EMBED_FILES "c6-loader.bin"
                       PRIV_REQUIRES esp_driver_uart esp_driver_gpio nvs_flash esp_event)
