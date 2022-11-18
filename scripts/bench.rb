#!/usr/bin/env ruby

# frozen_string_literal: true

# Script to test various configurations that can impact performance of the node

require "fileutils"
require "open3"
require "optparse"
require "pathname"
require "pp"
require "tmpdir"
require "toml-rb"

# Defines some hardcoded constants

Snapshot = "2322240_2022_11_09T06_00_00Z.car"

# This is just for capturing the snapshot height
Snapshot_regex = /(?<height>\d+)_.*/

Heights_to_validate = 400

Minute = 60
Hour = Minute * Minute

Benchmark_suite = [
  {
    :name => "baseline",
    :build_command => "cargo build --release",
    :import_command => "./target/release/forest --config %{c} --target-peer-count 50 --encrypt-keystore false --import-snapshot %{s} --halt-after-import",
    :validate_command => "./target/release/forest --config %{c} --target-peer-count 50 --encrypt-keystore false --import-snapshot %{s} --halt-after-import --skip-load --height %{h}",
    :config => {
      "rocks_db" => {
        "enable_statistics" => true,
      },
    },
  },
  # {
  #   :name => "baseline-with-jemalloc",
  #   :build_command => "cargo build --release --features 'rocksdb/jemalloc'",
  #   :import_command => "./target/release/forest --config %{c} --target-peer-count 50 --encrypt-keystore false --import-snapshot %{s} --halt-after-import",
  #   :validate_command => "./target/release/forest --config %{c} --target-peer-count 50 --encrypt-keystore false --import-snapshot %{s} --halt-after-import --skip-load --height %{h}",
  #   :config => {},
  # },
  {
    :name => "aggresive-rocksdb",
    :build_command => "cargo build --release",
    :import_command => "./target/release/forest --config %{c} --target-peer-count 50 --encrypt-keystore false --import-snapshot %{s} --halt-after-import",
    :validate_command => "./target/release/forest --config %{c} --target-peer-count 50 --encrypt-keystore false --import-snapshot %{s} --halt-after-import --skip-load --height %{h}",
    :config => {
      "rocks_db" => {
        "write_buffer_size" => 1024 * 1024 * 1024, # 1Gb memtable, will create as large L0 sst files
        "max_open_files" => -1,
        "compaction_style" => "none",
        "compression_type" => "none",
        "enable_statistics" => true,
        "optimize_for_point_lookup" => 256,
      },
    },
  },
]

$tmp_dir = nil

def tmp_dir()
  if !$tmp_dir
    $tmp_dir = Dir.mktmpdir("forest-benchs-")
  end
  $tmp_dir
end

def get_forest_version()
  version = exec_command("./target/release/forest --version", quiet: true)
  version.chomp
end

def get_default_config()
  toml_str = exec_command("./target/release/forest-cli config dump", quiet: true)

  default = TomlRB.parse(toml_str)
  default["client"]["data_dir"] = tmp_dir()
  default
end

def get_snapshot_dir()
  snapshot_dir = exec_command("./target/release/forest-cli snapshot dir", quiet: true)
  snapshot_dir.chomp
end

def get_db_dir()
  # TODO: expose chain as a script parameter, by default it should be mainnet
  config = get_default_config()
  data_dir = config.dig("client", "data_dir")

  "#{data_dir}/mainnet/db"
end

def get_db_size()
  size = exec_command("du -h '#{get_db_dir()}'", quiet: true)
  size.chomp.split[0]
end

def hr(seconds)
  seconds = seconds < Minute ? seconds.ceil(1) : seconds.ceil(0)
  time = Time.at(seconds)
  durfmt = "#{seconds > Hour ? "%-Hh" : ""}#{seconds < Minute ? "" : "%-Mm"}%-S#{seconds < Minute ? ".%1L" : ""}s"
  time.strftime(durfmt)
end

def mem_monitor(pid)
  rss_serie = []
  vsz_serie = []
  handle = Thread.new() {
    loop do
      sleep 0.5
      code = 0
      Open3.popen2("ps -o rss,vsz #{pid}", {}) { |i, o, t|
        i.close
        code = t.value
        if code == 0
          output = o.read.lines.last.split
          rss_serie.push output[0].to_i
          vsz_serie.push output[1].to_i
        end
      }
      if code != 0
        break
      end
    end
  }
  return handle, { :rss => rss_serie, :vsz => vsz_serie }
end

def exec_command(command, quiet: false, merge: false, dry_run: false)
  metrics = {}
  t0 = Process.clock_gettime(Process::CLOCK_MONOTONIC)
  if dry_run
    puts "$ #{command}"
  else
    # TODO: handle merge?
    opts = merge ? { :err => [:child, :out] } : {}
    Open3.popen2("#{command}", {}) { |i, o, t|
      handle, mem = mem_monitor(t.pid)
      i.close
      if quiet
        return o.read
      else
        puts "$ #{command}"
        o.each_line do |l|
          print l
        end
      end
      handle.join
      metrics.merge!(mem)
    }
  end
  t1 = Process.clock_gettime(Process::CLOCK_MONOTONIC)
  metrics[:elapsed] = hr(t1 - t0)
  metrics
end

def config_path(bench)
  "#{tmp_dir()}/#{bench[:name]}.toml"
end

def build_config_file(bench)
  default = get_default_config()
  bench_config = bench[:config]
  # TODO: Find a better way to merge (conserve the default keys)
  default.merge!(bench_config)

  toml_str = TomlRB.dump(default)
  File.open("#{config_path(bench)}", "w") { |file| file.write(toml_str) }
end

def build_substitution_hash(bench, options)
  snapshot = options.fetch(:snapshot, Snapshot)
  height = snapshot.match(Snapshot_regex).named_captures["height"].to_i
  start = height - options.fetch(:height, Heights_to_validate)

  # Escape spaces if any
  config_path = config_path(bench).gsub(/\s/, '\\ ')
  snapshot_path = "#{get_snapshot_dir()}/#{snapshot}".gsub(/\s/, '\\ ')

  { c: config_path, s: snapshot_path, h: start }
end

def write_result(metrics)
  result = "Bench | Import Time | Import RSS | DB Size\n"
  result += "-|-|-|-\n"

  metrics.each do |key, value|
    elapsed = value[:import][:elapsed]
    rss = value[:import][:rss]
    rss_max = rss ? "#{rss.max()}B" : "n/a"
    db_size = value[:import][:db_size] || "n/a"
    result += "#{key} | #{elapsed} | #{rss_max} | #{db_size}\n"
  end

  result += "---\n"

  result += "Bench | Validate Time | Validate RSS\n"
  result += "-|-|-\n"

  metrics.each do |key, value|
    elapsed = value[:validate][:elapsed]
    rss = value[:validate][:rss]
    rss_max = rss ? "#{rss.max()}B" : "n/a"
    result += "#{key} | #{elapsed} | #{rss_max}\n"
  end

  File.open("result.md", 'w') { |file| file.write(result) }
end

def run_benchmarks(benchs, options)
  benchs_metrics = {}
  benchs.each { |bench|
    puts "Running bench: #{bench[:name]}"

    metrics = {}
    metrics[:name] = bench[:name]

    dry_run = options[:dry_run]

    # TODO: cargo clean before
    exec_command(bench[:build_command], quiet: false, dry_run: dry_run)

    # Clean db
    db_dir = get_db_dir()
    puts "Wiping #{db_dir}"
    if !dry_run
      FileUtils.rm_rf(db_dir, :secure => true)
    end

    # Build bench artefacts
    #puts "Snapshot dir: #{get_snapshot_dir()}/#{Snapshot}"
    build_config_file(bench)
    params = build_substitution_hash(bench, options)

    # Run forest benchmark
    puts "Version: #{get_forest_version()}"

    import_command = bench[:import_command] % params
    metrics[:import] = exec_command(import_command, quiet: false, dry_run: dry_run)

    # Save db size just after import
    if !dry_run
      metrics[:import][:db_size] = get_db_size()
    end

    validate_command = bench[:validate_command] % params
    metrics[:validate] = exec_command(validate_command, quiet: false, dry_run: dry_run)

    benchs_metrics[bench[:name]] = metrics

    puts "\n"
  }
  write_result(benchs_metrics)
end

# TODO: read script arguments and do some filtering otherwise run them all

options = {}
OptionParser.new do |opts|
  opts.banner = "Usage: bench.rb [options]"
  opts.on("--dry-run", "Only print the commands that will be run") { |v| options[:dry_run] = v }
  opts.on("--snapshot [Object]", Object, "Snapshot file to use for benchmarks") { |v| options[:snapshot] = v }
  opts.on("--height [Integer]", Integer, "Number of heights to validate") { |v| options[:height] = v }
end.parse!

run_benchmarks(Benchmark_suite, options)

puts "Done."
