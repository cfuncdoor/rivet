cmake_minimum_required(VERSION 3.16)
project(mini CXX)

add_library(mini STATIC src/mini.cpp)
target_include_directories(mini PUBLIC include)
