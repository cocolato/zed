use std::{
    fmt::Debug,
    marker::PhantomData,
    ops::{Add, AddAssign, Sub, SubAssign},
};
use text::{Point, PointUtf16};

#[repr(transparent)]
pub struct TypedOffset<T> {
    pub offset: usize,
    _marker: PhantomData<T>,
}

#[repr(transparent)]
pub struct TypedPoint<T> {
    pub point: Point,
    _marker: PhantomData<T>,
}

#[repr(transparent)]
pub struct TypedPointUtf16<T> {
    pub point: PointUtf16,
    _marker: PhantomData<T>,
}

#[repr(transparent)]
pub struct TypedRow<T> {
    pub row: u32,
    _marker: PhantomData<T>,
}

impl<T> TypedOffset<T> {
    pub fn new(offset: usize) -> Self {
        Self {
            offset,
            _marker: PhantomData,
        }
    }
}
impl<T> TypedPoint<T> {
    pub fn new(row: u32, column: u32) -> Self {
        Self {
            point: Point::new(row, column),
            _marker: PhantomData,
        }
    }
    pub fn with(point: Point) -> Self {
        Self {
            point,
            _marker: PhantomData,
        }
    }
}
impl<T> TypedPointUtf16<T> {
    pub fn new(row: u32, column: u32) -> Self {
        TypedPointUtf16 {
            point: PointUtf16::new(row, column),
            _marker: PhantomData,
        }
    }
    pub fn with(point: PointUtf16) -> Self {
        Self {
            point,
            _marker: PhantomData,
        }
    }
}
impl<T> TypedRow<T> {
    pub fn new(row: u32) -> Self {
        Self {
            row,
            _marker: PhantomData,
        }
    }
}

impl<T> Copy for TypedOffset<T> {}
impl<T> Copy for TypedPoint<T> {}
impl<T> Copy for TypedPointUtf16<T> {}
impl<T> Copy for TypedRow<T> {}

impl<T> Clone for TypedOffset<T> {
    fn clone(&self) -> Self {
        Self {
            offset: self.offset,
            _marker: PhantomData,
        }
    }
}
impl<T> Clone for TypedPoint<T> {
    fn clone(&self) -> Self {
        Self {
            point: self.point,
            _marker: PhantomData,
        }
    }
}
impl<T> Clone for TypedPointUtf16<T> {
    fn clone(&self) -> Self {
        Self {
            point: self.point,
            _marker: PhantomData,
        }
    }
}
impl<T> Clone for TypedRow<T> {
    fn clone(&self) -> Self {
        Self {
            row: self.row,
            _marker: PhantomData,
        }
    }
}

impl<T> Default for TypedOffset<T> {
    fn default() -> Self {
        Self::new(0)
    }
}
impl<T> Default for TypedPoint<T> {
    fn default() -> Self {
        Self::with(Point::default())
    }
}
impl<T> Default for TypedRow<T> {
    fn default() -> Self {
        Self::new(0)
    }
}

impl<T> PartialOrd for TypedOffset<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.offset.cmp(&other.offset))
    }
}
impl<T> PartialOrd for TypedPoint<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.point.cmp(&other.point))
    }
}
impl<T> PartialOrd for TypedPointUtf16<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.point.cmp(&other.point))
    }
}
impl<T> PartialOrd for TypedRow<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.row.cmp(&other.row))
    }
}

impl<T> Ord for TypedOffset<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.offset.cmp(&other.offset)
    }
}
impl<T> Ord for TypedPoint<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.point.cmp(&other.point)
    }
}
impl<T> Ord for TypedPointUtf16<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.point.cmp(&other.point)
    }
}
impl<T> Ord for TypedRow<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.row.cmp(&other.row)
    }
}

impl<T> PartialEq for TypedOffset<T> {
    fn eq(&self, other: &Self) -> bool {
        self.offset == other.offset
    }
}
impl<T> PartialEq for TypedPoint<T> {
    fn eq(&self, other: &Self) -> bool {
        self.point == other.point
    }
}
impl<T> PartialEq for TypedPointUtf16<T> {
    fn eq(&self, other: &Self) -> bool {
        self.point == other.point
    }
}
impl<T> PartialEq for TypedRow<T> {
    fn eq(&self, other: &Self) -> bool {
        self.row == other.row
    }
}

impl<T> Eq for TypedOffset<T> {}
impl<T> Eq for TypedPoint<T> {}
impl<T> Eq for TypedPointUtf16<T> {}
impl<T> Eq for TypedRow<T> {}

impl<T> Debug for TypedOffset<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}Offset({})", type_name::<T>(), self.offset)
    }
}
impl<T> Debug for TypedPoint<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}Point({}, {})",
            type_name::<T>(),
            self.point.row,
            self.point.column
        )
    }
}
impl<T> Debug for TypedRow<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}Row({})", type_name::<T>(), self.row)
    }
}

fn type_name<T>() -> &'static str {
    std::any::type_name::<T>().split("::").last().unwrap()
}

impl<T> Add<TypedOffset<T>> for TypedOffset<T> {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        TypedOffset::new(self.offset + other.offset)
    }
}
impl<T> Add<TypedPoint<T>> for TypedPoint<T> {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        TypedPoint::with(self.point + other.point)
    }
}

impl<T> Sub<TypedOffset<T>> for TypedOffset<T> {
    type Output = Self;
    fn sub(self, other: Self) -> Self {
        TypedOffset::new(self.offset - other.offset)
    }
}
impl<T> Sub<TypedPoint<T>> for TypedPoint<T> {
    type Output = Self;
    fn sub(self, other: Self) -> Self {
        TypedPoint::with(self.point - other.point)
    }
}

impl<T> AddAssign<TypedOffset<T>> for TypedOffset<T> {
    fn add_assign(&mut self, other: Self) {
        self.offset += other.offset;
    }
}
impl<T> AddAssign<TypedPoint<T>> for TypedPoint<T> {
    fn add_assign(&mut self, other: Self) {
        self.point += other.point;
    }
}

impl<T> SubAssign<Self> for TypedOffset<T> {
    fn sub_assign(&mut self, other: Self) {
        self.offset -= other.offset;
    }
}
impl<T> SubAssign<Self> for TypedRow<T> {
    fn sub_assign(&mut self, other: Self) {
        self.row -= other.row;
    }
}
