#!/usr/bin/env python3
"""
简单的控制工具 - 通过网络控制 Pluto SDR 参数
"""

import socket
import sys
import argparse

def send_command(host, port, command):
    """发送命令到服务器"""
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.connect((host, port))
        sock.send(f"{command}\n".encode())
        
        response = sock.recv(1024).decode().strip()
        print(response)
        
        sock.close()
        return True
    except Exception as e:
        print(f"错误: {e}", file=sys.stderr)
        return False

def main():
    parser = argparse.ArgumentParser(
        description='Pluto SDR 控制工具',
        epilog='示例:\n'
               '  %(prog)s -f 145.8    # 设置频率为 145.8 MHz\n'
               '  %(prog)s -g 60       # 设置增益为 60 dB\n'
               '  %(prog)s -s          # 查询状态',
        formatter_class=argparse.RawDescriptionHelpFormatter
    )
    
    parser.add_argument('--host', default='localhost', help='服务器地址 (默认: localhost)')
    parser.add_argument('--port', type=int, default=5556, help='控制端口 (默认: 5556)')
    parser.add_argument('-f', '--freq', type=float, metavar='MHz', help='设置中心频率 (MHz)')
    parser.add_argument('-g', '--gain', type=float, metavar='dB', help='设置增益 (dB)')
    parser.add_argument('-s', '--status', action='store_true', help='查询当前状态')
    parser.add_argument('-c', '--command', help='发送自定义命令')
    
    args = parser.parse_args()
    
    # 构建命令
    if args.freq is not None:
        command = f"f {args.freq}"
    elif args.gain is not None:
        command = f"g {args.gain}"
    elif args.status:
        command = "s"
    elif args.command:
        command = args.command
    else:
        parser.print_help()
        return 1
    
    # 发送命令
    success = send_command(args.host, args.port, command)
    return 0 if success else 1

if __name__ == '__main__':
    sys.exit(main())
