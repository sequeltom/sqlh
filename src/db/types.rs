//! Column metadata to `Column`, with SQL type names as they'd be declared.

use mssql_tds::datatypes::sqldatatypes::TdsDataType;
use mssql_tds::query::metadata::ColumnMetadata;

use super::Column;

pub fn column(metadata: &ColumnMetadata) -> Column {
    Column {
        name: metadata.column_name.clone(),
        type_name: type_name(metadata),
        nullable: metadata.is_nullable(),
    }
}

fn type_name(md: &ColumnMetadata) -> String {
    use TdsDataType as T;
    let len = md.type_info.length;
    let sized = |base: &str, n: usize| {
        if md.is_plp() {
            format!("{base}(max)")
        } else {
            format!("{base}({n})")
        }
    };
    let scaled = |base: &str| format!("{base}({})", md.get_scale().unwrap_or(7));
    let exact = |base: &str| {
        format!(
            "{base}({},{})",
            md.get_precision().unwrap_or(18),
            md.get_scale().unwrap_or(0)
        )
    };
    let name = match md.data_type {
        T::Int1 => "tinyint",
        T::Int2 => "smallint",
        T::Int4 => "int",
        T::Int8 => "bigint",
        T::IntN => match len {
            1 => "tinyint",
            2 => "smallint",
            4 => "int",
            _ => "bigint",
        },
        T::Bit | T::BitN => "bit",
        T::Flt4 => "real",
        T::Flt8 => "float",
        T::FltN => {
            if len == 4 {
                "real"
            } else {
                "float"
            }
        }
        T::Money => "money",
        T::Money4 => "smallmoney",
        T::MoneyN => {
            if len == 4 {
                "smallmoney"
            } else {
                "money"
            }
        }
        T::DateTime => "datetime",
        T::DateTim4 => "smalldatetime",
        T::DateTimeN => {
            if len == 4 {
                "smalldatetime"
            } else {
                "datetime"
            }
        }
        T::Decimal | T::DecimalN => return exact("decimal"),
        T::Numeric | T::NumericN => return exact("numeric"),
        T::DateN => "date",
        T::TimeN => return scaled("time"),
        T::DateTime2N => return scaled("datetime2"),
        T::DateTimeOffsetN => return scaled("datetimeoffset"),
        T::BigVarChar | T::VarChar => return sized("varchar", len),
        T::BigChar | T::Char => return sized("char", len),
        T::NVarChar => return sized("nvarchar", len / 2),
        T::NChar => return sized("nchar", len / 2),
        T::BigVarBinary | T::VarBinary => return sized("varbinary", len),
        T::BigBinary | T::Binary => return sized("binary", len),
        T::Text => "text",
        T::NText => "ntext",
        T::Image => "image",
        T::Xml => "xml",
        T::Guid => "uniqueidentifier",
        T::SsVariant => "sql_variant",
        T::Json => "json",
        T::Vector => "vector",
        T::Udt => {
            return md
                .type_info
                .udt_info()
                .map(|u| u.type_name().to_owned())
                .unwrap_or_else(|| "udt".into());
        }
        other => return format!("{other:?}").to_lowercase(),
    };
    name.to_owned()
}
